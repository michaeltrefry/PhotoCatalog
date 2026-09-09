use anyhow::{Context, Result, ensure};
use xmp_toolkit::{FromStrOptions, ToStringOptions, XmpMeta, XmpValue};

const XMP: &str = "http://ns.adobe.com/xap/1.0/";
const UNKNOWN: &str = "https://example.invalid/photocatalog/unknown/";

const QUALIFIED_PACKET: &str = r#"<?xpacket begin="﻿" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
<rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/"
 xmlns:u="https://example.invalid/photocatalog/unknown/">
 <xmp:Rating rdf:parseType="Resource"><rdf:value>2</rdf:value><u:confidence>source-specific</u:confidence></xmp:Rating>
 <u:history><rdf:Seq><rdf:li rdf:parseType="Resource"><u:step>first</u:step><u:data rdf:parseType="Resource"><u:value>雪 &amp; light</u:value></u:data></rdf:li><rdf:li rdf:parseType="Resource"><u:step>second</u:step></rdf:li></rdf:Seq></u:history>
 <u:alternatives><rdf:Alt><rdf:li xml:lang="x-default">original</rdf:li><rdf:li xml:lang="fr">conservé</rdf:li></rdf:Alt></u:alternatives>
</rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end="w"?>"#;

fn canonical(meta: &mut XmpMeta) -> Result<String> {
    meta.sort().context("sort complete XMP model")?;
    Ok(meta
        .to_string_with_options(
            ToStringOptions::default()
                .omit_packet_wrapper()
                .use_canonical_format()
                .omit_all_formatting(),
        )
        .context("serialize complete XMP model")?)
}

#[test]
fn changing_a_scalar_preserves_its_unknown_qualifier_and_unrelated_model() -> Result<()> {
    let mut meta = XmpMeta::from_str_with_options(
        QUALIFIED_PACKET,
        FromStrOptions::default().strict_aliasing(),
    )?;
    let original = canonical(&mut meta)?;
    let previous = meta.property(XMP, "Rating").expect("retained rating");
    ensure!(previous.value == "2" && previous.has_qualifiers());
    let updated = XmpValue::new("4".to_owned()).set_is_uri(previous.is_uri());
    meta.set_property(XMP, "Rating", &updated)
        .context("set qualified rating")?;
    let serialized = canonical(&mut meta)?;
    let mut reparsed =
        XmpMeta::from_str_with_options(&serialized, FromStrOptions::default().strict_aliasing())?;
    ensure!(reparsed.property(XMP, "Rating").unwrap().value == "4");
    ensure!(
        reparsed
            .qualifier(XMP, "Rating", UNKNOWN, "confidence")
            .unwrap()
            .value
            == "source-specific"
    );
    // Reversing only the intended scalar change must reproduce the complete model.
    // This compares a fallible full serialization, not a potentially partial iterator.
    reparsed
        .set_property(
            XMP,
            "Rating",
            &XmpValue::new(previous.value.clone()).set_is_uri(previous.is_uri()),
        )
        .context("restore qualified rating")?;
    ensure!(canonical(&mut reparsed)? == original);
    Ok(())
}

#[test]
fn public_edits_preserve_nested_data_and_language_siblings() -> Result<()> {
    use photocatalog::xmp::{self, Edit, Value};
    let original = QUALIFIED_PACKET.as_bytes();
    let edits = vec![
        Edit::Set {
            namespace: XMP.into(),
            path: "Rating".into(),
            value: "5".into(),
        },
        Edit::Localized {
            namespace: xmp::DC.into(),
            path: "title".into(),
            language: "fr".into(),
            value: "Neige".into(),
        },
        Edit::Localized {
            namespace: xmp::DC.into(),
            path: "title".into(),
            language: "en".into(),
            value: "Snow".into(),
        },
        Edit::Append {
            namespace: xmp::DC.into(),
            path: "subject".into(),
            value: "winter".into(),
            ordered: false,
        },
        Edit::Append {
            namespace: xmp::DC.into(),
            path: "subject".into(),
            value: "mountain".into(),
            ordered: false,
        },
    ];
    let output = xmp::apply_edits(original, &edits)?;
    let projected = xmp::project(&output)?;
    ensure!(projected.issues.is_empty(), "{:?}", projected.issues);
    ensure!(projected.fields["rating"] == Value::Text("5".into()));
    let Value::Localized(title) = &projected.fields["title"] else {
        panic!("localized title")
    };
    ensure!(title["fr"] == "Neige" && title["en"] == "Snow" && title["x-default"] == "Neige");
    let undo = vec![
        Edit::Set {
            namespace: XMP.into(),
            path: "Rating".into(),
            value: "2".into(),
        },
        Edit::Remove {
            namespace: xmp::DC.into(),
            path: "title".into(),
        },
        Edit::Remove {
            namespace: xmp::DC.into(),
            path: "subject".into(),
        },
    ];
    ensure!(
        xmp::canonical(&xmp::parse(&xmp::apply_edits(&output, &undo)?)?)?
            == xmp::canonical(&xmp::parse(original)?)?
    );
    Ok(())
}

#[test]
fn strict_transport_decoding_and_structure_guards() -> Result<()> {
    use photocatalog::xmp::{self, Edit};
    let expected = xmp::canonical(&xmp::parse(QUALIFIED_PACKET.as_bytes())?)?;
    for little in [false, true] {
        let mut utf16 = if little {
            vec![0xff, 0xfe]
        } else {
            vec![0xfe, 0xff]
        };
        for unit in QUALIFIED_PACKET.encode_utf16() {
            utf16.extend(if little {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        ensure!(xmp::canonical(&xmp::parse(&utf16)?)? == expected);
        let mut utf32 = if little {
            vec![0xff, 0xfe, 0, 0]
        } else {
            vec![0, 0, 0xfe, 0xff]
        };
        for c in QUALIFIED_PACKET.chars() {
            utf32.extend(if little {
                (c as u32).to_le_bytes()
            } else {
                (c as u32).to_be_bytes()
            });
        }
        ensure!(xmp::canonical(&xmp::parse(&utf32)?)? == expected);
    }
    ensure!(xmp::parse(b"<!DOCTYPE a [<!ENTITY x SYSTEM 'file:///x'>]><a>&x;</a>").is_err());
    ensure!(xmp::parse(&[0xff, 0xfe, 0]).is_err());
    ensure!(
        xmp::apply_edits(
            QUALIFIED_PACKET.as_bytes(),
            &[Edit::Set {
                namespace: UNKNOWN.into(),
                path: "history".into(),
                value: "flattened".into()
            }]
        )
        .is_err()
    );
    Ok(())
}
