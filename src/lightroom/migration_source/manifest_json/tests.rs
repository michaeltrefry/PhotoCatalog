use super::*;
use crate::lightroom::migration_source::tests::Fixture;

fn with_output(template: &Manifest, output: &str) -> Result<Vec<u8>> {
    let raw = serde_json::to_string(template)?;
    let old = format!(
        "\"output\":{}",
        serde_json::to_string(&template.request.output)?
    );
    assert!(raw.contains(&old));
    Ok(raw
        .replacen(&old, &format!("\"output\":{output}"), 1)
        .into_bytes())
}
fn parity(raw: &[u8]) {
    let old = serde_json::from_slice::<Manifest>(raw);
    let new = decode(raw);
    assert_eq!(old.is_ok(), new.is_ok(), "old={old:?}; new={new:?}");
    if let (Ok(old), Ok(new)) = (old, new) {
        assert_eq!(
            serde_json::to_vec(&old).unwrap(),
            serde_json::to_vec(&new).unwrap()
        );
    }
}

#[test]
fn direct_paths_preserve_order_units_unknown_duplicate_and_numeric_semantics() -> Result<()> {
    let fixture = Fixture::new();
    let manifest = fixture.open().capture_manifest(fixture.revision())?;
    for output in [
        r#"{"encoding":"UnixBytes","units":[0,255]}"#,
        r#"{"units":[0,65535,55296],"encoding":"WindowsWide","foreign":{"__proto__":true}}"#,
        r#"["WindowsWide",[0,65535]]"#,
        r#"{"units":[],"encoding":"UnixBytes","units":[1]}"#,
        r#"{"encoding":"UnixBytes","encoding":"WindowsWide","units":[1]}"#,
        r#"{"encoding":"unknown","units":[]}"#,
        r#"{"encoding":{"UnixBytes":null},"units":[]}"#,
        r#"{"encoding":0,"units":[]}"#,
        r#"{"encoding":null,"units":[]}"#,
        r#"{"encoding":"\u0055nixBytes","units":[255]}"#,
        r#"{"encoding":"UnixBytes"}"#,
        r#"{"units":[256],"encoding":"UnixBytes"}"#,
        r#"{"units":[65536],"encoding":"WindowsWide"}"#,
        r#"{"units":["0"],"encoding":"UnixBytes"}"#,
        r#"{"units":[-1],"encoding":"UnixBytes"}"#,
        r#"{"units":[1.0],"encoding":"UnixBytes"}"#,
        r#"{"units":[1e0],"encoding":"UnixBytes"}"#,
        r#"{"units":null,"encoding":"UnixBytes"}"#,
    ] {
        parity(&with_output(&manifest, output)?);
    }
    let raw = serde_json::to_string(&manifest)?;
    for number in [
        "null".to_string(),
        "1".into(),
        u64::MAX.to_string(),
        u128::MAX.to_string(),
    ] {
        parity(
            raw.replace("\"modified_ns\":null", &format!("\"modified_ns\":{number}"))
                .as_bytes(),
        );
    }
    parity(
        raw.replacen('{', r#"{"unknown":[{"__proto__":null}],"#, 1)
            .as_bytes(),
    );
    parity(
        raw.replacen("\"protocol\":1", "\"protocol\":1,\"protocol\":1", 1)
            .as_bytes(),
    );
    parity(
        raw.replacen("\"state\":\"captured\"", "\"state\":null", 1)
            .as_bytes(),
    );
    Ok(())
}

#[test]
fn units_before_tag_borrow_input_until_concrete_numeric_dispatch() -> Result<()> {
    let raw = format!(
        r#"{{"units":[{}],"encoding":"WindowsWide"}}"#,
        "65535,".repeat(4096).trim_end_matches(',')
    );
    let path: Path<'_> = serde_json::from_str(&raw)?;
    let pointer = path.units.get().as_ptr() as usize;
    assert!((raw.as_ptr() as usize..raw.as_ptr() as usize + raw.len()).contains(&pointer));
    let NativePath::WindowsWide(units) = path.decode()? else {
        panic!("foreign evidence preserved")
    };
    assert_eq!(units, vec![65535; 4096]);
    Ok(())
}

#[test]
fn sequence_minima_optional_tails_and_nested_forms_match_original_types() -> Result<()> {
    fn same<T: serde::de::DeserializeOwned + serde::Serialize>(raw: &str) -> Result<usize> {
        let value: T = serde_json::from_str(raw)?;
        let canonical = serde_json::to_vec(&value)?;
        eprintln!(
            "shape={} retained={} canonical={}",
            std::any::type_name::<T>(),
            raw.len(),
            canonical.len()
        );
        Ok(raw.len())
    }
    assert_eq!(same::<Issue>(r#"["",null,""]"#)?, 12);
    assert_eq!(same::<NativePath>(r#"["UnixBytes",[]]"#)?, 16);
    assert_eq!(same::<Revision>(r#"["",0,null,""]"#)?, 14);
    assert_eq!(
        same::<Artifact>(r#"[["UnixBytes",[]],"",["UnixBytes",[]],"",["",0,null,""],""]"#)?,
        59
    );
    // true is one byte shorter than false, and both remain ordinary evidence.
    assert_eq!(
        same::<Entry>(r#"[["UnixBytes",[]],"",["UnixBytes",[]],true,null,""]"#)?,
        51
    );
    let fixture = Fixture::new();
    let manifest = fixture.open().capture_manifest(fixture.revision())?;
    let object = serde_json::to_value(&manifest)?;
    let fields = [
        "protocol",
        "request",
        "state",
        "raw_byte_retention",
        "sqlite_consistency",
        "application_consistency",
        "cooperative_lock_protocol",
        "artifacts",
        "companion_inventory",
        "absent_companions",
        "issues",
        "wal",
        "logical_blake3",
        "logical_revision",
        "revision_id",
    ];
    for length in 0..=fields.len() {
        let sequence: Vec<_> = fields[..length]
            .iter()
            .map(|key| object[*key].clone())
            .collect();
        parity(&serde_json::to_vec(&sequence)?);
    }
    let mut nested = object.clone();
    nested["issues"] = serde_json::from_str(r#"[["",null,""]]"#)?;
    nested["companion_inventory"] =
        serde_json::from_str(r#"[[["UnixBytes",[]],"",["UnixBytes",[]],true,null,""]]"#)?;
    nested["absent_companions"] = serde_json::from_str(r#"[["UnixBytes",[]]]"#)?;
    nested["logical_revision"] = serde_json::from_str(r#"["",0,null,""]"#)?;
    nested["unknown"] = serde_json::from_str(r#"{"deep":[{"unknown":[0,null,{}]}]}"#)?;
    parity(&serde_json::to_vec(&nested)?);
    eprintln!(
        "{}",
        serde_json::json!({"capacity_type_bytes": {
        "manifest":std::mem::size_of::<Manifest>(), "manifest_json":std::mem::size_of::<ManifestJson<'_>>(),
        "artifact":std::mem::size_of::<Artifact>(), "artifact_json":std::mem::size_of::<ArtifactJson<'_>>(),
        "entry":std::mem::size_of::<Entry>(), "entry_json":std::mem::size_of::<EntryJson<'_>>(),
        "path_json":std::mem::size_of::<Path<'_>>(), "native_path":std::mem::size_of::<NativePath>(),
        "issue":std::mem::size_of::<Issue>(), "string":std::mem::size_of::<String>(),
        "value":std::mem::size_of::<serde_json::Value>() }})
    );
    Ok(())
}

// Part B grammar proof: these are typed JSON representations, not admitted
// capture/seal authority. No filesystem, Plan or MigrationSource is opened.
const B_PATH: &str = r#"["UnixBytes",[]]"#;
const B_REVISION: &str = r#"["",0,0,""]"#;

fn b_request() -> String {
    format!(r#"[{B_PATH},{B_PATH},true,"",[0,0,0,0,0]]"#)
}
fn b_manifest() -> String {
    format!(
        r#"[0,{},"","","","","",[],[],[],[],null,"",null,""]"#,
        b_request()
    )
}
fn b_expected_manifest(raw: &[u8], accepted: bool) {
    let original = serde_json::from_slice::<Manifest>(raw);
    let private = decode(raw);
    assert_eq!(original.is_ok(), accepted, "original: {original:?}");
    assert_eq!(private.is_ok(), accepted, "private: {private:?}");
    if let (Ok(original), Ok(private)) = (original, private) {
        assert_eq!(
            serde_json::to_vec(&original).unwrap(),
            serde_json::to_vec(&private).unwrap()
        );
    }
}

#[test]
fn b_grammar_some_values_and_nested_sequences_establish_shorter_candidates() -> Result<()> {
    let issue_raw = r#"["","",""]"#;
    let issue: Issue = serde_json::from_str(issue_raw)?;
    assert_eq!(issue.source_id.as_deref(), Some(""));
    assert_eq!(issue_raw.len(), 10);
    assert_eq!(
        serde_json::from_str::<Issue>(r#"["",null,""]"#)?.source_id,
        None
    );

    let revision: Revision = serde_json::from_str(B_REVISION)?;
    assert_eq!(revision.modified_ns, Some(0));
    assert_eq!(B_REVISION.len(), 11);
    assert_eq!(
        serde_json::from_str::<Revision>(r#"["",0,null,""]"#)?.modified_ns,
        None
    );

    let path: NativePath = serde_json::from_str(B_PATH)?;
    assert_eq!(path, NativePath::UnixBytes(vec![]));
    assert_eq!(B_PATH.len(), 16);
    let wide_raw = r#"["WindowsWide",[]]"#;
    assert_eq!(
        serde_json::from_str::<NativePath>(wide_raw)?,
        NativePath::WindowsWide(vec![])
    );
    assert_eq!(wide_raw.len(), 18);

    let artifact_raw = format!(r#"[{B_PATH},"",{B_PATH},"",{B_REVISION},""]"#);
    let artifact: Artifact = serde_json::from_str(&artifact_raw)?;
    assert_eq!(artifact_raw.len(), 56);
    assert_eq!(artifact.revision.modified_ns, Some(0));
    let entry_raw = format!(r#"[{B_PATH},"",{B_PATH},true,0,""]"#);
    let entry: Entry = serde_json::from_str(&entry_raw)?;
    assert_eq!(entry_raw.len(), 48);
    assert_eq!(entry.modified_ns, Some(0));

    let request: Request = serde_json::from_str(&b_request())?;
    assert_eq!(request.closed_application_evidence.as_deref(), Some(""));
    assert_eq!(b_request().len(), 55);
    assert!(serde_json::from_str::<Limits>("[0,0,0,0,0]").is_ok());
    assert!(serde_json::from_str::<WalReport>("[0,0,0,0,0,0,0]").is_ok());
    b_expected_manifest(b_manifest().as_bytes(), true);

    // All shorter optional representations occur together in the actual
    // Manifest view, rather than only deserializing isolated member types.
    let mut object = serde_json::to_value(serde_json::from_str::<Manifest>(&b_manifest())?)?;
    object["issues"] = serde_json::from_str(&format!("[{issue_raw}]"))?;
    object["artifacts"] = serde_json::from_str(&format!("[{artifact_raw}]"))?;
    object["companion_inventory"] = serde_json::from_str(&format!("[{entry_raw}]"))?;
    object["absent_companions"] = serde_json::from_str(&format!("[{wide_raw}]"))?;
    object["logical_revision"] = serde_json::from_str(B_REVISION)?;
    b_expected_manifest(&serde_json::to_vec(&object)?, true);
    eprintln!(
        "B accepted candidate bytes: path=16 wide_path=18 revision_some0=11 artifact=56 entry_some0=48 issue_some_empty=10 request=55; array_separator_costs=57/49/17/11; no heap/global maximum qualification"
    );
    Ok(())
}

#[test]
fn b_grammar_map_option_omission_and_explicit_sequence_defaults_are_distinct() -> Result<()> {
    let full: Vec<serde_json::Value> = serde_json::from_str(&b_manifest())?;
    assert_eq!(full.len(), 15);
    for length in 0..=full.len() {
        b_expected_manifest(&serde_json::to_vec(&full[..length])?, length == 15);
    }
    let mut extra = full;
    extra.push(serde_json::Value::Null);
    b_expected_manifest(&serde_json::to_vec(&extra)?, false);
    let mut object = serde_json::to_value(serde_json::from_str::<Manifest>(&b_manifest())?)?;
    for optional in ["wal", "logical_blake3", "logical_revision", "revision_id"] {
        assert!(object.as_object_mut().unwrap().remove(optional).is_some());
        b_expected_manifest(&serde_json::to_vec(&object)?, true);
    }
    object.as_object_mut().unwrap().remove("issues");
    b_expected_manifest(&serde_json::to_vec(&object)?, false);

    use crate::xmp_packets::SourceRevision;
    assert!(serde_json::from_str::<SourceRevision>(r#"[7,"retained"]"#).is_err());
    assert_eq!(
        serde_json::from_str::<SourceRevision>(r#"[7,"retained",null]"#)?.modified_unix_ns,
        None
    );
    assert_eq!(
        serde_json::from_str::<SourceRevision>(r#"{"length":7,"blake3":"retained"}"#)?
            .modified_unix_ns,
        None
    );
    assert!(serde_json::from_str::<Issue>(r#"["",""]"#).is_err());
    assert!(serde_json::from_str::<Revision>(r#"["",0,0]"#).is_err());
    assert!(serde_json::from_str::<Limits>("[0,0,0,0]").is_err());
    assert!(serde_json::from_str::<WalReport>("[0,0,0,0,0,0]").is_err());

    // A real explicitly defaulted field is the contrast, not an invented
    // assumption that every trailing Option can be omitted from a sequence.
    use crate::lightroom::migration_source::InputSeal;
    let seal_sequence = format!(r#"[1,{B_PATH},{B_REVISION},"",["","",""],[],[]]"#);
    let seal: InputSeal = serde_json::from_str(&seal_sequence)?;
    assert!(seal.supplements.is_empty());
    let mut sequence: Vec<serde_json::Value> = serde_json::from_str(&seal_sequence)?;
    sequence.pop();
    assert!(serde_json::from_slice::<InputSeal>(&serde_json::to_vec(&sequence)?).is_err());
    Ok(())
}

#[test]
fn b_grammar_native_tag_forms_preserve_original_sequence_rejection() -> Result<()> {
    let private =
        |raw: &str| -> Result<NativePath> { serde_json::from_str::<Path<'_>>(raw)?.decode() };
    for raw in [
        r#"{"encoding":"UnixBytes","units":[]}"#,
        r#"{"encoding":{"UnixBytes":null},"units":[]}"#,
        r#"{"units":[55296,65535,0],"encoding":{"WindowsWide":null}}"#,
        B_PATH,
        r#"["WindowsWide",[55296,65535,0]]"#,
        r#"{"units":[],"encoding":"\u0055nixBytes","ignored":{"__proto__":0}}"#,
    ] {
        let original = serde_json::from_str::<NativePath>(raw)?;
        assert_eq!(original, private(raw)?);
    }
    let padded = format!(
        r#"{{"encoding":{{"UnixBytes":{}null{}}},"units":[]}}"#,
        " ".repeat(4096),
        " ".repeat(4096)
    );
    assert_eq!(
        serde_json::from_str::<NativePath>(&padded)?,
        private(&padded)?
    );
    for raw in [
        r#"[0,[]]"#,
        r#"{"encoding":0,"units":[]}"#,
        r#"["unix_bytes",[]]"#,
        r#"["Unix",[]]"#,
        r#"["windows_wide",[]]"#,
        r#"{"encoding":{"UnixBytes":null,"WindowsWide":null},"units":[]}"#,
        r#"{"encoding":"UnixBytes","units":[256]}"#,
        r#"{"encoding":"WindowsWide","units":[65536]}"#,
        r#"{"encoding":"UnixBytes","units":[-1]}"#,
        r#"{"encoding":"UnixBytes","units":[1.0]}"#,
        r#"{"encoding":"UnixBytes","units":[],"encoding":"UnixBytes"}"#,
    ] {
        assert!(
            serde_json::from_str::<NativePath>(raw).is_err(),
            "original accepted {raw}"
        );
        assert!(private(raw).is_err(), "private accepted {raw}");
    }

    // v27 preserved the old private widening as a passing discrepancy proof.
    // B now requires the original public rejection at both private boundaries.
    let template: Manifest = serde_json::from_str(&b_manifest())?;
    for raw in [
        r#"[{"UnixBytes":null},[]]"#,
        r#"[{"WindowsWide":null},[55296,65535,0]]"#,
    ] {
        assert!(serde_json::from_str::<NativePath>(raw).is_err());
        assert!(private(raw).is_err());
        let bytes = with_output(&template, raw)?;
        assert!(serde_json::from_slice::<Manifest>(&bytes).is_err());
        assert!(decode(&bytes).is_err());
    }

    Ok(())
}

#[test]
fn b_grammar_concrete_structs_keep_raw_keys_unknown_fields_and_full_scalar_ranges() -> Result<()> {
    let template: Manifest = serde_json::from_str(&b_manifest())?;
    let raw = serde_json::to_string(&template)?;
    let special = "$serde_json::private::RawValue";
    let wrapped = format!(r#"{{"{special}":{}}}"#, serde_json::to_string(&raw)?);
    b_expected_manifest(wrapped.as_bytes(), false);
    // This concrete type ignores an unknown first key; unlike the supplement's
    // Value stage it neither opens nor interprets the reserved string payload.
    let unknown = raw.replacen('{', &format!(r#"{{"{special}":"not JSON","#,), 1);
    b_expected_manifest(unknown.as_bytes(), true);
    for ignored in [
        "1e400".to_owned(),
        format!("{}0{}", "[".repeat(256), "]".repeat(256)),
    ] {
        let unknown = raw.replacen('{', &format!(r#"{{"ignored":{ignored},"#), 1);
        b_expected_manifest(unknown.as_bytes(), true);
    }
    let duplicate = raw.replacen("\"protocol\":0", "\"protocol\":0,\"protocol\":0", 1);
    assert_ne!(duplicate, raw);
    b_expected_manifest(duplicate.as_bytes(), false);
    for number in [
        "null".to_owned(),
        "0".into(),
        u64::MAX.to_string(),
        u128::MAX.to_string(),
    ] {
        let revision = format!(
            r#"{{"object":"","bytes":18446744073709551615,"modified_ns":{number},"changed":""}}"#
        );
        let changed = raw.replace(
            "\"logical_revision\":null",
            &format!("\"logical_revision\":{revision}"),
        );
        assert_ne!(changed, raw);
        b_expected_manifest(changed.as_bytes(), true);
    }
    for number in [
        "340282366920938463463374607431768211456",
        "-1",
        "0.0",
        "1e400",
    ] {
        let revision = format!(r#"{{"object":"","bytes":0,"modified_ns":{number},"changed":""}}"#);
        b_expected_manifest(
            raw.replace(
                "\"logical_revision\":null",
                &format!("\"logical_revision\":{revision}"),
            )
            .as_bytes(),
            false,
        );
    }
    Ok(())
}

#[test]
fn b_exact_capacity_projection_has_no_retained_row_staging_vectors() -> Result<()> {
    let mut manifest: Manifest = serde_json::from_str(&b_manifest())?;
    for _ in 0..4097 {
        manifest.issues.push(Issue {
            code: "".into(),
            source_id: Some("".into()),
            detail: "".into(),
        });
        manifest
            .absent_companions
            .push(NativePath::WindowsWide(vec![0, 55296, 65535]));
    }
    let bytes = serde_json::to_vec(&manifest)?;
    let digest = blake3::hash(&bytes);
    let decoded = decode(&bytes)?;
    assert_eq!(serde_json::to_vec(&decoded)?, bytes);
    assert_eq!(decoded.issues.len(), decoded.issues.capacity());
    assert_eq!(
        decoded.absent_companions.len(),
        decoded.absent_companions.capacity()
    );
    for path in &decoded.absent_companions {
        let NativePath::WindowsWide(units) = path else {
            panic!("foreign evidence changed")
        };
        assert_eq!(units.len(), units.capacity());
    }
    assert_eq!(blake3::hash(&bytes), digest);
    let calls = std::cell::Cell::new(0);
    assert!(
        decode_reply(&bytes, &|| {
            let n = calls.get() + 1;
            calls.set(n);
            n > 128
        })
        .unwrap_err()
        .to_string()
        .contains("canceled")
    );
    eprintln!(
        "B exact projection: issues_len={} capacity={} paths_len={} capacity={} original_bytes={} unchanged_digest={}",
        decoded.issues.len(),
        decoded.issues.capacity(),
        decoded.absent_companions.len(),
        decoded.absent_companions.capacity(),
        bytes.len(),
        digest
    );
    Ok(())
}

#[test]
fn b_source_floor_keeps_four_separator_slack_and_rejects_reply_amplification() -> Result<()> {
    // Desktop 64-bit values, and any smaller target layout, fit these public
    // row-size ratios. This covers requested payload capacity, not allocator RSS.
    assert!(std::mem::size_of::<Artifact>() * 11 <= 57 * 72);
    assert!(std::mem::size_of::<Entry>() * 11 <= 49 * 72);
    assert!(std::mem::size_of::<NativePath>() * 11 <= 17 * 72);
    assert!(std::mem::size_of::<Issue>() <= 72);

    let mut f = admission::Footprint::default();
    f.member(crate::lightroom::MANIFEST_BYTES + 4)?;
    assert!(f.member(1).is_err());
    let mut manifest: Manifest = serde_json::from_str(&b_manifest())?;
    manifest.state = "x".repeat(crate::lightroom::MANIFEST_BYTES + 5);
    let bytes = serde_json::to_vec(&manifest)?;
    assert!(bytes.len() < 97 * 1024 * 1024);
    assert!(
        decode_reply(&bytes, &|| false)
            .unwrap_err()
            .to_string()
            .contains("retained byte limit")
    );
    Ok(())
}
