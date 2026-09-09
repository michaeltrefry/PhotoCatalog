use anyhow::{Context, Result, ensure};
use xmp_toolkit::{FromStrOptions, ToStringOptions, XmpMeta, XmpValue};

const XMP: &str = "http://ns.adobe.com/xap/1.0/";
const UNKNOWN: &str = "https://example.invalid/photocatalog/unknown/";

#[test]
fn editing_preserves_original_rdf_before_adobe_legacy_repairs() -> Result<()> {
    use photocatalog::xmp::{self, Edit};
    const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
    const MM: &str = "http://ns.adobe.com/xap/1.0/mm/";
    let original = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="uuid:11111111-2222-3333-4444-555555555555" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:xmpMM="http://ns.adobe.com/xap/1.0/mm/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:u="https://example.invalid/photocatalog/unknown/" xmlns:xmpDM="http://ns.adobe.com/xmp/1.0/DynamicMedia/">
    <xmp:Rating>2</xmp:Rating>
    <xmpMM:InstanceID rdf:parseType="Resource"><rdf:value>xmp.iid:existing</rdf:value><u:proof>retained</u:proof></xmpMM:InstanceID>
    <dc:title><rdf:Alt><rdf:li xml:lang="x-default">Default</rdf:li><rdf:li xml:lang="fr">Bonjour</rdf:li></rdf:Alt></dc:title>
    <dc:creator>scalar creator</dc:creator>
    <dc:subject><rdf:Seq><rdf:li>second</rdf:li><rdf:li>first</rdf:li></rdf:Seq></dc:subject>
    <dc:description><rdf:Alt><rdf:li rdf:parseType="Resource"><u:opaque>structured alternative</u:opaque></rdf:li></rdf:Alt></dc:description>
    <xmpDM:copyright>audio rights</xmpDM:copyright>
    </rdf:Description></rdf:RDF></x:xmpmeta>"#;
    let output = xmp::apply_edits(
        original,
        &[Edit::Set {
            namespace: XMP.into(),
            path: "Rating".into(),
            value: "4".into(),
        }],
    )?;
    // These assertions read serialized RDF directly. An SDK-normalized baseline
    // would hide destructive migrations that happened before the edit began.
    let text = std::str::from_utf8(&output)?;
    let document = roxmltree::Document::parse(text)?;
    let description = document
        .descendants()
        .find(|n| n.has_tag_name((RDF, "Description")))
        .unwrap();
    ensure!(
        description.attribute((RDF, "about")) == Some("uuid:11111111-2222-3333-4444-555555555555")
    );
    let instance = document
        .descendants()
        .find(|n| n.has_tag_name((MM, "InstanceID")))
        .unwrap();
    ensure!(
        instance
            .descendants()
            .any(|n| n.has_tag_name((RDF, "value")) && n.text() == Some("xmp.iid:existing"))
    );
    ensure!(
        instance
            .descendants()
            .any(|n| n.has_tag_name((UNKNOWN, "proof")) && n.text() == Some("retained"))
    );
    let title = document
        .descendants()
        .find(|n| n.has_tag_name((xmp::DC, "title")))
        .unwrap();
    ensure!(
        title
            .descendants()
            .any(|n| n.attribute((xmp::XML, "lang")) == Some("fr") && n.text() == Some("Bonjour"))
    );
    let meta = xmp::parse(&output)?;
    ensure!(meta.property(xmp::DC, "creator").unwrap().value == "scalar creator");
    ensure!(!meta.property(xmp::DC, "creator").unwrap().is_array());
    ensure!(meta.property(xmp::DC, "subject").unwrap().is_ordered());
    ensure!(
        document
            .descendants()
            .any(|n| (n.has_tag_name((UNKNOWN, "opaque"))
                && n.text() == Some("structured alternative"))
                || n.attribute((UNKNOWN, "opaque")) == Some("structured alternative"))
    );
    ensure!(
        meta.property("http://ns.adobe.com/xmp/1.0/DynamicMedia/", "copyright")
            .unwrap()
            .value
            == "audio rights"
    );
    let localized = xmp::apply_edits(
        &output,
        &[Edit::Localized {
            namespace: xmp::DC.into(),
            path: "title".into(),
            language: "fr".into(),
            value: "Bonsoir".into(),
        }],
    )?;
    let model = xmp::parse(&localized)?;
    ensure!(
        model
            .property(xmp::DC, "title[?xml:lang='x-default']")
            .unwrap()
            .value
            == "Default"
    );
    ensure!(
        model
            .property(xmp::DC, "title[?xml:lang='fr']")
            .unwrap()
            .value
            == "Bonsoir"
    );
    Ok(())
}

#[test]
fn aliases_and_historical_properties_are_literal_independent_data() -> Result<()> {
    use photocatalog::xmp::{self, Edit};
    const PDF: &str = "http://ns.adobe.com/pdf/1.3/";
    const IX: &str = "http://ns.adobe.com/iX/1.0/";
    let original = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:pdf="http://ns.adobe.com/pdf/1.3/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:iX="http://ns.adobe.com/iX/1.0/"><pdf:Author>PDF author</pdf:Author><dc:creator><rdf:Seq><rdf:li>DC author</rdf:li></rdf:Seq></dc:creator><iX:changes><rdf:Seq><rdf:li>historical value</rdf:li></rdf:Seq></iX:changes></rdf:Description></rdf:RDF></x:xmpmeta>"#;
    let output = xmp::apply_edits(
        original,
        &[Edit::Set {
            namespace: PDF.into(),
            path: "Author".into(),
            value: "edited PDF author".into(),
        }],
    )?;
    let meta = xmp::parse(&output)?;
    ensure!(meta.property(PDF, "Author").unwrap().value == "edited PDF author");
    ensure!(meta.property(xmp::DC, "creator[1]").unwrap().value == "DC author");
    ensure!(meta.property(IX, "changes[1]").unwrap().value == "historical value");
    Ok(())
}

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
    meta.to_string_with_options(
        ToStringOptions::default()
            .omit_packet_wrapper()
            .use_canonical_format()
            .omit_all_formatting(),
    )
    .context("serialize complete XMP model")
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

#[test]
fn qualified_keyword_items_survive_append_and_scalar_change() -> Result<()> {
    use photocatalog::xmp::{self, Edit};
    let original=br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:u="https://example.invalid/unknown/"><dc:subject><rdf:Bag><rdf:li rdf:parseType="Resource"><rdf:value>snow</rdf:value><u:confidence>0.9</u:confidence></rdf:li></rdf:Bag></dc:subject></rdf:Description></rdf:RDF></x:xmpmeta>"#;
    let output = xmp::apply_edits(
        original,
        &[
            Edit::Set {
                namespace: xmp::DC.into(),
                path: "subject[1]".into(),
                value: "winter".into(),
            },
            Edit::Append {
                namespace: xmp::DC.into(),
                path: "subject".into(),
                value: "mountain".into(),
                ordered: false,
            },
        ],
    )?;
    let meta = xmp::parse(&output)?;
    ensure!(
        meta.qualifier(
            xmp::DC,
            "subject[1]",
            "https://example.invalid/unknown/",
            "confidence"
        )
        .unwrap()
        .value
            == "0.9"
    );
    let reversed = xmp::apply_edits(
        &output,
        &[
            Edit::Remove {
                namespace: xmp::DC.into(),
                path: "subject[2]".into(),
            },
            Edit::Set {
                namespace: xmp::DC.into(),
                path: "subject[1]".into(),
                value: "snow".into(),
            },
        ],
    )?;
    ensure!(xmp::canonical(&xmp::parse(&reversed)?)? == xmp::canonical(&xmp::parse(original)?)?);
    Ok(())
}

#[test]
fn malformed_transport_declarations_are_not_normalized_into_valid_xmp() -> Result<()> {
    use photocatalog::xmp;
    for declaration in [
        "<?xml THIS IS NOT A DECLARATION?>",
        "<?xml encoding='UTF-16'?>",
        "<?xml version='1.0' unexpected='yes'?>",
    ] {
        let text = format!("{declaration}{QUALIFIED_PACKET}");
        let mut utf16 = vec![255, 254];
        for unit in text.encode_utf16() {
            utf16.extend(unit.to_le_bytes());
        }
        ensure!(xmp::parse(&utf16).is_err());
        let mut utf32 = vec![0, 0, 254, 255];
        for c in text.chars() {
            utf32.extend((c as u32).to_be_bytes());
        }
        ensure!(xmp::parse(&utf32).is_err());
    }
    let text = format!("<?xml version='1.0' encoding='UTF-16'?>{QUALIFIED_PACKET}");
    let mut bytes = vec![255, 254];
    for unit in text.encode_utf16() {
        bytes.extend(unit.to_le_bytes());
    }
    ensure!(
        xmp::canonical(&xmp::parse(&bytes)?)?
            == xmp::canonical(&xmp::parse(QUALIFIED_PACKET.as_bytes())?)?
    );
    Ok(())
}

#[test]
fn empty_model_subject_survives_no_change_export_and_property_removal() -> Result<()> {
    use photocatalog::xmp;
    let packet=br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="urn:review:subject"/></rdf:RDF></x:xmpmeta>"#;
    let output = xmp::reconcile_fields(packet, &[])?;
    ensure!(xmp::parse(&output)?.name() == "urn:review:subject");
    ensure!(xmp::canonical(&xmp::parse(packet)?)? == xmp::canonical(&xmp::parse(&output)?)?);
    Ok(())
}
