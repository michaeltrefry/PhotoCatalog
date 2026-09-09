#[path = "../src/xmp_rdf.rs"]
mod xmp_rdf;
use xmp_rdf::assert_equivalent;

fn packet(body: &str) -> String {
    format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:u="urn:unknown:" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:mm="http://ns.adobe.com/xap/1.0/mm/">{body}</rdf:Description></rdf:RDF></x:xmpmeta>"#
    )
}
fn equal(a: &str, b: &str) {
    assert_equivalent(&packet(a), &packet(b)).unwrap();
}
fn different(a: &str, b: &str) {
    assert!(assert_equivalent(&packet(a), &packet(b)).is_err());
}

#[test]
fn namespace_prefixes_compact_properties_and_struct_forms_are_equivalent() {
    let compact = r#"<r:RDF xmlns:r="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:p="urn:unknown:"><r:Description r:about="" p:Title="a &amp; b"><p:Record p:One="snow" p:Two="雪"/></r:Description></r:RDF>"#;
    let expanded = packet(
        "<u:Title>a &amp; b</u:Title><u:Record rdf:parseType='Resource'><u:Two>雪</u:Two><u:One>snow</u:One></u:Record>",
    );
    assert_equivalent(compact, &expanded).unwrap();
    equal(
        "<u:Record u:One='snow'/>",
        "<u:Record><rdf:Description><u:One>snow</u:One></rdf:Description></u:Record>",
    );
    different(
        "<u:Title>value</u:Title>",
        "<v:Title xmlns:v='urn:other:'>value</v:Title>",
    );
}

#[test]
fn unknown_qualified_values_and_uri_forms_are_preserved() {
    equal(
        "<u:Link rdf:resource='https://example.invalid/' u:confidence='0.9'/>",
        "<u:Link rdf:parseType='Resource'><rdf:value rdf:resource='https://example.invalid/'/><u:confidence>0.9</u:confidence></u:Link>",
    );
    equal(
        "<u:Rating rdf:value='3' u:note='keep'/>",
        "<u:Rating><rdf:Description><rdf:value>3</rdf:value><u:note>keep</u:note></rdf:Description></u:Rating>",
    );
    different(
        "<u:Rating rdf:value='3' u:note='keep'/>",
        "<u:Rating>3</u:Rating>",
    );
    different("<u:Link rdf:resource='target'/>", "<u:Link>target</u:Link>");
    equal(
        "<u:Record><u:Kind u:field='text'/></u:Record>",
        "<u:Record rdf:parseType='Resource'><rdf:value rdf:parseType='Resource'><u:field>text</u:field></rdf:value><rdf:type rdf:resource='urn:unknown:Kind'/></u:Record>",
    );
}

#[test]
fn bag_order_is_semantic_free_but_duplicates_sequence_and_form_are_preserved() {
    equal(
        "<u:Tags><rdf:Bag><rdf:li>a</rdf:li><rdf:li>b</rdf:li><rdf:li>a</rdf:li></rdf:Bag></u:Tags>",
        "<u:Tags><rdf:Bag><rdf:li>a</rdf:li><rdf:li>a</rdf:li><rdf:li>b</rdf:li></rdf:Bag></u:Tags>",
    );
    different(
        "<u:Tags><rdf:Bag><rdf:li>a</rdf:li><rdf:li>a</rdf:li></rdf:Bag></u:Tags>",
        "<u:Tags><rdf:Bag><rdf:li>a</rdf:li></rdf:Bag></u:Tags>",
    );
    different(
        "<u:Tags><rdf:Seq><rdf:li>a</rdf:li><rdf:li>b</rdf:li></rdf:Seq></u:Tags>",
        "<u:Tags><rdf:Seq><rdf:li>b</rdf:li><rdf:li>a</rdf:li></rdf:Seq></u:Tags>",
    );
    different(
        "<dc:subject><rdf:Seq><rdf:li>a</rdf:li></rdf:Seq></dc:subject>",
        "<dc:subject><rdf:Bag><rdf:li>a</rdf:li></rdf:Bag></dc:subject>",
    );
}

#[test]
fn two_language_values_cannot_be_overwritten_and_language_case_is_equivalent() {
    let before = "<dc:title><rdf:Alt><rdf:li xml:lang='x-default'>Default</rdf:li><rdf:li xml:lang='fr-FR'>Bonjour</rdf:li></rdf:Alt></dc:title>";
    let reordered = "<dc:title><rdf:Alt><rdf:li xml:lang='FR-fr'>Bonjour</rdf:li><rdf:li xml:lang='x-default'>Default</rdf:li></rdf:Alt></dc:title>";
    equal(before, reordered);
    different(before, &before.replace("Bonjour", "Default"));
}

#[test]
fn malformed_alt_text_children_and_unknown_qualifiers_cannot_disappear() {
    let before = "<dc:description><rdf:Alt><rdf:li rdf:parseType='Resource'><u:deep>opaque</u:deep></rdf:li><rdf:li/><rdf:li xml:lang='en' u:note='keep' rdf:value='text'/></rdf:Alt></dc:description>";
    equal(before, before);
    different(
        before,
        "<dc:description><rdf:Alt><rdf:li xml:lang='en'>text</rdf:li></rdf:Alt></dc:description>",
    );
    different(before, &before.replace("<rdf:li/>", ""));
}

#[test]
fn legacy_uuid_and_existing_instance_id_are_independent() {
    let original = packet("<mm:InstanceID>xmp.iid:keep</mm:InstanceID>")
        .replace("rdf:about=\"\"", "rdf:about=\"uuid:source\"");
    assert_equivalent(&original, &original).unwrap();
    assert!(
        assert_equivalent(
            &original,
            &packet("<mm:InstanceID>uuid:source</mm:InstanceID>")
        )
        .is_err()
    );
    let empty = packet("").replace("rdf:about=\"\"", "rdf:about=\"urn:subject\"");
    assert!(assert_equivalent(&empty, &packet("")).is_err());
}

#[test]
fn legacy_properties_and_explicit_aliases_are_not_silently_migrated() {
    let old = "<iX:changes xmlns:iX='http://ns.adobe.com/iX/1.0/'><rdf:Bag><rdf:li>retained</rdf:li></rdf:Bag></iX:changes>";
    equal(old, old);
    different(old, "");
    different(
        "<dc:creator>author</dc:creator>",
        "<dc:creator><rdf:Seq><rdf:li>author</rdf:li></rdf:Seq></dc:creator>",
    );
    let aliases = "<xmp:Author>alias</xmp:Author><dc:creator><rdf:Seq><rdf:li>actual</rdf:li></rdf:Seq></dc:creator>";
    equal(aliases, aliases);
    different(
        aliases,
        "<dc:creator><rdf:Seq><rdf:li>actual</rdf:li></rdf:Seq></dc:creator>",
    );
    different(
        "<dm:copyright xmlns:dm='http://ns.adobe.com/xmp/1.0/DynamicMedia/'>keep</dm:copyright>",
        "<dc:rights><rdf:Alt><rdf:li xml:lang='x-default'>keep</rdf:li></rdf:Alt></dc:rights>",
    );
}

#[test]
fn unsupported_rdf_graph_constructs_are_explicit_even_without_a_change() {
    for body in [
        "<u:Value rdf:datatype='http://www.w3.org/2001/XMLSchema#integer'>3</u:Value>",
        "<u:Value rdf:nodeID='node'/>",
        "<u:Value rdf:ID='identity'>3</u:Value>",
        "<u:Record><rdf:Description rdf:about='urn:nested'><u:Name>value</u:Name></rdf:Description></u:Record>",
        "<u:Tags><rdf:Seq><rdf:_2>b</rdf:_2><rdf:_1>a</rdf:_1></rdf:Seq></u:Tags>",
    ] {
        let input = packet(body);
        assert!(
            assert_equivalent(&input, &input).is_err(),
            "unsupported graph accepted: {body}"
        );
    }
}

#[test]
fn malformed_duplicate_mixed_or_excessive_inputs_are_refused() {
    for input in [
        packet("<u:Value>unterminated"),
        packet("<u:Value>a</u:Value><u:Value>b</u:Value>"),
        packet("<u:Record rdf:parseType='Resource'>text<u:Name>value</u:Name></u:Record>"),
        packet(&format!(
            "{}text{}",
            "<u:n rdf:parseType='Resource'>".repeat(66),
            "</u:n>".repeat(66)
        )),
        "x".repeat(16 * 1024 * 1024 + 1),
    ] {
        assert!(assert_equivalent(&input, &input).is_err());
    }
    different("<u:Value> value </u:Value>", "<u:Value>value</u:Value>");
    equal(
        "<u:Value>snow &amp; <![CDATA[sun]]></u:Value>",
        "<u:Value>snow &amp; sun</u:Value>",
    );
}

#[test]
fn inherited_context_non_xml_whitespace_and_duplicate_languages_are_explicit() {
    for attribute in ["xml:base='https://example.invalid/'", "xml:lang='fr'"] {
        let source = packet("<u:Link rdf:resource='relative'/>")
            .replace("<x:xmpmeta ", &format!("<x:xmpmeta {attribute} "));
        assert!(assert_equivalent(&source, &source).is_err());
    }
    for content in ["\u{a0}", "\u{2003}"] {
        let source = packet(content);
        assert!(assert_equivalent(&source, &source).is_err());
    }
    let duplicate = packet(
        "<dc:title><rdf:Alt><rdf:li xml:lang='fr-FR'>un</rdf:li><rdf:li xml:lang='FR-fr'>deux</rdf:li></rdf:Alt></dc:title>",
    );
    assert!(assert_equivalent(&duplicate, &duplicate).is_err());
}
