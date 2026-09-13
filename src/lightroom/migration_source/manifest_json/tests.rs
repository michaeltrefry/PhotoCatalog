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
