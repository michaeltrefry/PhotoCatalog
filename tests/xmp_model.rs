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
