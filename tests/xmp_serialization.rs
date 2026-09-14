use xmp_toolkit::{ToStringOptions, XmpMeta, XmpValue};

#[test]
fn bounded_sdk_serialization_preserves_bytes_and_retries_after_refusal() -> anyhow::Result<()> {
    let mut meta = XmpMeta::new()?;
    meta.set_property(
        photocatalog::xmp::XMP,
        "Label",
        &XmpValue::new("café 📷 <&>".to_owned()),
    )?;
    let options = || ToStringOptions::default().use_compact_format();
    let expected = meta.to_string_with_options(options())?;
    assert!(!expected.is_empty());
    assert!(
        meta.to_string_with_options_bounded(options(), expected.len() - 1)?
            .is_none()
    );
    assert!(meta.to_string_with_options_bounded(options(), 0)?.is_none());
    assert_eq!(
        meta.to_string_with_options_bounded(options(), expected.len())?,
        Some(expected)
    );
    let invalid = || {
        ToStringOptions::default()
            .exact_packet_length()
            .set_padding(1)
    };
    let expected_error = meta.to_string_with_options(invalid()).unwrap_err();
    assert_eq!(
        meta.to_string_with_options_bounded(invalid(), 0)
            .unwrap_err(),
        expected_error,
    );
    assert!(
        meta.to_string_with_options_bounded(options(), usize::MAX)?
            .is_some()
    );
    Ok(())
}
