use anyhow::{Context, Result, ensure};
use photocatalog::xmp::{self, Edit, XMP};
use xmp_toolkit::{ToStringOptions, XmpMeta, XmpValue};

const UNKNOWN: &str = "urn:photocatalog:synthetic-empty-scalar";

fn qualified(field: &str, value: &str) -> Vec<u8> {
    format!(r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:u="urn:photocatalog:synthetic-empty-scalar"><xmp:{field} rdf:parseType="Resource"><rdf:value>{value}</rdf:value><u:proof>keep</u:proof></xmp:{field}><u:unknown rdf:parseType="Resource"><u:nested>雪 &amp; intact</u:nested></u:unknown></rdf:Description></rdf:RDF></x:xmpmeta>"#).into_bytes()
}

fn set(field: &str, value: &str) -> Edit {
    Edit::Set {
        namespace: XMP.into(),
        path: field.into(),
        value: value.into(),
    }
}

#[test]
fn empty_to_nonempty_and_back_preserves_scalar_qualifiers_and_unknown_model() -> Result<()> {
    // The first case reproduces the failed migration's ordinary Label edit:
    // forward Set succeeds, then the preservation check restores the empty value.
    for (field, old, new) in [
        ("Label", "", "Red"),
        ("Label", "Red", ""),
        ("Rating", "", "0"),
        ("Rating", "0", ""),
    ] {
        let original = qualified(field, old);
        let before = xmp::parse(&original)?;
        let property = before.property(XMP, field).context("original scalar")?;
        ensure!(!property.is_array() && !property.is_struct() && property.value == old);
        let changed = xmp::apply_edits(&original, &[set(field, new)])
            .with_context(|| format!("set {field} from {old:?} to {new:?}"))?;
        let after = xmp::parse(&changed)?;
        ensure!(after.property(XMP, field).context("updated scalar")?.value == new);
        ensure!(
            after
                .qualifier(XMP, field, UNKNOWN, "proof")
                .context("preserved qualifier")?
                .value
                == "keep"
        );
        let restored = xmp::apply_edits(&changed, &[set(field, old)])?;
        ensure!(xmp::canonical(&xmp::parse(&restored)?)? == xmp::canonical(&before)?);
    }
    Ok(())
}

#[test]
fn absent_empty_and_explicit_removal_remain_distinct() -> Result<()> {
    let absent = xmp::empty_packet()?;
    ensure!(xmp::parse(&absent)?.property(XMP, "Label").is_none());
    let present = xmp::apply_edits(&absent, &[set("Label", "")])?;
    ensure!(
        xmp::parse(&present)?
            .property(XMP, "Label")
            .context("present empty scalar")?
            .value
            .is_empty()
    );
    ensure!(xmp::canonical(&xmp::parse(&present)?)? != xmp::canonical(&xmp::parse(&absent)?)?);
    let removed = xmp::apply_edits(
        &present,
        &[Edit::Remove {
            namespace: XMP.into(),
            path: "Label".into(),
        }],
    )?;
    ensure!(xmp::canonical(&xmp::parse(&removed)?)? == xmp::canonical(&xmp::parse(&absent)?)?);
    Ok(())
}

#[test]
fn explicit_composite_constructors_and_scalar_replacement_guards_are_unchanged() -> Result<()> {
    let mut model = XmpMeta::new()?;
    model.set_property(
        XMP,
        "FixtureStruct",
        &XmpValue::new(String::new()).set_is_struct(true),
    )?;
    model.set_property(
        XMP,
        "FixtureArray",
        &XmpValue::new(String::new()).set_is_array(true),
    )?;
    ensure!(
        model
            .property(XMP, "FixtureStruct")
            .context("empty struct")?
            .is_struct()
    );
    ensure!(
        model
            .property(XMP, "FixtureArray")
            .context("empty array")?
            .is_array()
    );
    let packet = model.to_string_with_options(ToStringOptions::default().use_compact_format())?;
    for field in ["FixtureStruct", "FixtureArray"] {
        let error = xmp::apply_edits(packet.as_bytes(), &[set(field, "replacement")]).unwrap_err();
        ensure!(
            error
                .to_string()
                .contains("scalar edit would replace a structured value")
        );
        ensure!(
            model
                .set_property(
                    XMP,
                    field,
                    &XmpValue::new("invalid composite value".into()).set_is_array(true)
                )
                .is_err()
        );
    }
    ensure!(
        model.to_string_with_options(ToStringOptions::default().use_compact_format())? == packet
    );
    Ok(())
}

#[test]
fn empty_array_leaf_set_and_append_preserve_other_items_and_qualifiers() -> Result<()> {
    let original = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:u="urn:photocatalog:synthetic-empty-scalar"><xmp:FixtureArray><rdf:Bag><rdf:li rdf:parseType="Resource"><rdf:value></rdf:value><u:proof>keep</u:proof></rdf:li><rdf:li>other item</rdf:li></rdf:Bag></xmp:FixtureArray></rdf:Description></rdf:RDF></x:xmpmeta>"#;
    let changed = xmp::apply_edits(original, &[set("FixtureArray[1]", "new value")])?;
    let model = xmp::parse(&changed)?;
    ensure!(model.array_len(XMP, "FixtureArray") == 2);
    ensure!(
        model
            .array_item(XMP, "FixtureArray", 2)
            .context("untouched item")?
            .value
            == "other item"
    );
    ensure!(
        model
            .qualifier(XMP, "FixtureArray[1]", UNKNOWN, "proof")
            .context("item qualifier")?
            .value
            == "keep"
    );
    let restored = xmp::apply_edits(&changed, &[set("FixtureArray[1]", "")])?;
    ensure!(xmp::canonical(&xmp::parse(&restored)?)? == xmp::canonical(&xmp::parse(original)?)?);
    let appended = xmp::apply_edits(
        &restored,
        &[Edit::Append {
            namespace: XMP.into(),
            path: "FixtureArray".into(),
            value: String::new(),
            ordered: false,
        }],
    )?;
    let model = xmp::parse(&appended)?;
    ensure!(model.array_len(XMP, "FixtureArray") == 3);
    ensure!(
        model
            .array_item(XMP, "FixtureArray", 3)
            .context("appended empty item")?
            .value
            .is_empty()
    );
    Ok(())
}
