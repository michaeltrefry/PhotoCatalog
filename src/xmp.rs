//! Fallible XMP interpretation and explicit edits. Original packet bytes live separately.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use xmp_toolkit::{FromStrOptions, ToStringOptions, XmpMeta, XmpValue};

pub const XMP: &str = "http://ns.adobe.com/xap/1.0/";
pub const DC: &str = "http://purl.org/dc/elements/1.1/";
pub const XML: &str = "http://www.w3.org/XML/1998/namespace";
pub const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;
const MAX_ITEMS: usize = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Value {
    /// Explicit catalog removal, distinct from a source that never supplied this field.
    Removed,
    Text(String),
    List(Vec<String>),
    Localized(BTreeMap<String, String>),
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Projection {
    pub fields: BTreeMap<String, Value>,
    pub issues: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Edit {
    /// Set one simple property, array item or structure field. Existing qualifiers survive.
    Set {
        namespace: String,
        path: String,
        value: String,
    },
    /// Explicitly remove the selected subtree, including its qualifiers.
    Remove { namespace: String, path: String },
    /// Append a simple item, preserving existing items and their qualifiers.
    Append {
        namespace: String,
        path: String,
        value: String,
        ordered: bool,
    },
    /// Change exactly this language; other language alternatives remain unchanged.
    Localized {
        namespace: String,
        path: String,
        language: String,
        value: String,
    },
}

/// Strict UTF-8/16/32 decoding; retained bytes are never replaced by this parse representation.
pub fn xml_text(bytes: &[u8]) -> Result<String> {
    ensure!(
        bytes.len() <= MAX_PACKET_BYTES,
        "XMP packet exceeds parse limit"
    );
    let (skip, width, little) = if bytes.starts_with(&[0xff, 0xfe, 0, 0]) {
        (4, 4, true)
    } else if bytes.starts_with(&[0, 0, 0xfe, 0xff]) {
        (4, 4, false)
    } else if bytes.starts_with(&[0xff, 0xfe]) {
        (2, 2, true)
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        (2, 2, false)
    } else if bytes.starts_with(&[b'<', 0, 0, 0]) {
        (0, 4, true)
    } else if bytes.starts_with(&[0, 0, 0, b'<']) {
        (0, 4, false)
    } else if bytes.starts_with(&[b'<', 0]) {
        (0, 2, true)
    } else if bytes.starts_with(&[0, b'<']) {
        (0, 2, false)
    } else {
        (
            usize::from(bytes.starts_with(&[0xef, 0xbb, 0xbf])) * 3,
            1,
            false,
        )
    };
    let body = &bytes[skip..];
    let text = match width {
        1 => std::str::from_utf8(body)
            .context("XMP is not UTF-8")?
            .to_owned(),
        2 => {
            ensure!(body.len().is_multiple_of(2), "truncated UTF-16 XMP");
            let units = body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| {
                    if little {
                        u16::from_le_bytes([b[0], b[1]])
                    } else {
                        u16::from_be_bytes([b[0], b[1]])
                    }
                })
                .collect::<Vec<_>>();
            String::from_utf16(&units).context("invalid UTF-16 XMP")?
        }
        4 => {
            ensure!(body.len().is_multiple_of(4), "truncated UTF-32 XMP");
            body.as_chunks::<4>()
                .0
                .iter()
                .map(|b| {
                    let a = [b[0], b[1], b[2], b[3]];
                    char::from_u32(if little {
                        u32::from_le_bytes(a)
                    } else {
                        u32::from_be_bytes(a)
                    })
                    .context("invalid UTF-32 XMP")
                })
                .collect::<Result<String>>()?
        }
        _ => unreachable!(),
    };
    ensure!(!text.contains('\0'), "NUL in XMP text");
    ensure!(
        !text.contains("<!DOCTYPE") && !text.contains("<!ENTITY"),
        "DTD/entity declarations are not accepted in XMP"
    );
    Ok(text)
}
pub fn parse(bytes: &[u8]) -> Result<XmpMeta> {
    let text = xml_text(bytes)?;
    let document = roxmltree::Document::parse_with_options(
        &text,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 100_000,
            ..Default::default()
        },
    )
    .context("validate complete XMP XML")?;
    const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
    let roots = document
        .descendants()
        .filter(|n| n.has_tag_name((RDF, "RDF")))
        .collect::<Vec<_>>();
    ensure!(roots.len() == 1, "XMP must contain exactly one RDF model");
    for node in document.descendants().filter(|n| n.is_element()) {
        ensure!(
            node.ancestors().take(66).count() <= 65,
            "XMP nesting exceeds model limit"
        );
    }
    // Validate the original decoded declaration before normalizing transport.
    // The SDK receives UTF-8 bytes, independent of the original UTF-16/32 declaration.
    let sdk_text = if text.starts_with("<?xml ") {
        &text[text.find("?>").context("unterminated XML declaration")? + 2..]
    } else {
        &text
    };
    let model =
        XmpMeta::from_str_with_options(sdk_text, FromStrOptions::default().strict_aliasing())
            .context("parse XMP model")?;
    // Compare original RDF before trusting any native projection. The pinned
    // preservation feature prevents known migrations; this independent boundary
    // also detects unsupported graph semantics or future native normalization.
    let serialized = serialize(&model)?;
    crate::xmp_rdf::assert_equivalent(&text, std::str::from_utf8(&serialized)?)?;
    Ok(model)
}
fn serialize(meta: &XmpMeta) -> Result<Vec<u8>> {
    let bytes = meta
        .to_string_with_options(ToStringOptions::default().use_compact_format())?
        .into_bytes();
    ensure!(bytes.len() <= MAX_PACKET_BYTES, "edited XMP exceeds limit");
    Ok(bytes)
}
/// Full fallible serialization avoids the SDK wrapper's lossy Clone/iterator error paths.
pub fn canonical(meta: &XmpMeta) -> Result<String> {
    let mut copy = parse(&serialize(meta)?)?;
    copy.sort()?;
    Ok(copy.to_string_with_options(
        ToStringOptions::default()
            .omit_packet_wrapper()
            .use_canonical_format()
            .omit_all_formatting(),
    )?)
}

pub fn project(bytes: &[u8]) -> Result<Projection> {
    let meta = parse(bytes)?;
    let mut result = Projection::default();
    for (field, ns, path) in [
        ("rating", XMP, "Rating"),
        ("label", XMP, "Label"),
        (
            "capture_date",
            "http://ns.adobe.com/exif/1.0/",
            "DateTimeOriginal",
        ),
        ("camera_make", "http://ns.adobe.com/tiff/1.0/", "Make"),
        ("camera_model", "http://ns.adobe.com/tiff/1.0/", "Model"),
        (
            "orientation",
            "http://ns.adobe.com/tiff/1.0/",
            "Orientation",
        ),
        ("lens", "http://ns.adobe.com/exif/1.0/aux/", "Lens"),
        (
            "gps_latitude",
            "http://ns.adobe.com/exif/1.0/",
            "GPSLatitude",
        ),
        (
            "gps_longitude",
            "http://ns.adobe.com/exif/1.0/",
            "GPSLongitude",
        ),
        ("create_date", XMP, "CreateDate"),
        ("modify_date", XMP, "ModifyDate"),
    ] {
        if let Some(value) = meta.property(ns, path) {
            if value.is_array() || value.is_struct() {
                result.issues.push(format!(
                    "{field}: expected scalar; original structure retained"
                ));
            } else {
                if field == "rating"
                    && !value
                        .value
                        .parse::<i32>()
                        .is_ok_and(|n| (-1..=5).contains(&n))
                {
                    result
                        .issues
                        .push("rating: outside -1..=5; exact value retained".into());
                }
                result.fields.insert(field.into(), Value::Text(value.value));
            }
        }
    }
    for (field, ns, path, localized, unordered) in [
        ("title", DC, "title", true, false),
        ("description", DC, "description", true, false),
        ("rights", DC, "rights", true, false),
        ("creator", DC, "creator", false, false),
        ("keywords", DC, "subject", false, true),
        (
            "hierarchical_keywords",
            "http://ns.adobe.com/lightroom/1.0/",
            "hierarchicalSubject",
            false,
            true,
        ),
    ] {
        let Some(value) = meta.property(ns, path) else {
            continue;
        };
        if !value.is_array() {
            result
                .issues
                .push(format!("{field}: expected array; original retained"));
            continue;
        }
        let count = meta.array_len(ns, path);
        if count > MAX_ITEMS {
            result.issues.push(format!(
                "{field}: projection limit exceeded; full array retained"
            ));
            continue;
        }
        let mut values = Vec::new();
        let mut languages = BTreeMap::new();
        let mut valid = true;
        for index in 1..=count {
            let Some(item) = meta.array_item(ns, path, index as i32) else {
                valid = false;
                break;
            };
            if item.is_array() || item.is_struct() {
                valid = false;
                break;
            }
            if localized {
                let Some(language) = meta.qualifier(ns, &format!("{path}[{index}]"), XML, "lang")
                else {
                    valid = false;
                    break;
                };
                if languages.insert(language.value, item.value).is_some() {
                    valid = false;
                    break;
                }
            } else {
                values.push(item.value);
            }
        }
        if !valid {
            result.issues.push(format!(
                "{field}: non-simple or ambiguous items; full array retained"
            ));
            continue;
        }
        if unordered {
            values.sort();
        }
        result.fields.insert(
            field.into(),
            if localized {
                Value::Localized(languages)
            } else {
                Value::List(values)
            },
        );
    }
    Ok(result)
}

fn address(namespace: &str, path: &str) -> Result<()> {
    ensure!(
        !namespace.is_empty()
            && !path.is_empty()
            && !namespace.contains('\0')
            && !path.contains('\0'),
        "invalid XMP address"
    );
    if XmpMeta::namespace_prefix(namespace).is_none() {
        XmpMeta::register_namespace(namespace, "custom")?;
    }
    Ok(())
}
fn set_scalar(meta: &mut XmpMeta, ns: &str, path: &str, value: &str) -> Result<()> {
    ensure!(!value.contains('\0'), "NUL in metadata value");
    let before = canonical(meta)?;
    let old = meta.property(ns, path);
    ensure!(
        !old.as_ref().is_some_and(|p| p.is_struct() || p.is_array()),
        "scalar edit would replace a structured value"
    );
    meta.set_property(
        ns,
        path,
        &XmpValue::new(value.to_owned()).set_is_uri(old.as_ref().is_some_and(|p| p.is_uri())),
    )?;
    let mut check = parse(&serialize(meta)?)?;
    ensure!(
        check.property(ns, path).is_some_and(|p| p.value == value),
        "edited value did not round-trip"
    );
    if let Some(old) = old {
        check.set_property(
            ns,
            path,
            &XmpValue::new(old.value.clone()).set_is_uri(old.is_uri()),
        )?;
    } else {
        check.delete_property(ns, path)?;
    }
    ensure!(
        canonical(&check)? == before,
        "edit would alter unrelated XMP semantics"
    );
    Ok(())
}

pub fn apply_edits(bytes: &[u8], edits: &[Edit]) -> Result<Vec<u8>> {
    ensure!(edits.len() <= 1000, "too many metadata edits");
    apply_organization_edits(bytes, edits)
}
/// Organization may update each of the already bounded 10,000 array items. Keep
/// one native mutation pass: sorting between chunks would change item addresses.
pub(crate) fn apply_organization_edits(bytes: &[u8], edits: &[Edit]) -> Result<Vec<u8>> {
    ensure!(
        edits.len() <= MAX_ITEMS,
        "too many organization array edits"
    );
    let mut meta = parse(bytes)?;
    for edit in edits {
        match edit {
            Edit::Set {
                namespace,
                path,
                value,
            } => {
                address(namespace, path)?;
                set_scalar(&mut meta, namespace, path, value)?;
            }
            Edit::Remove { namespace, path } => {
                address(namespace, path)?;
                meta.delete_property(namespace, path)?;
            }
            Edit::Append {
                namespace,
                path,
                value,
                ordered,
            } => {
                address(namespace, path)?;
                ensure!(!value.contains('\0'), "NUL in metadata value");
                let original = canonical(&meta)?;
                let existing = meta.property(namespace, path);
                ensure!(
                    !existing.as_ref().is_some_and(|p| !p.is_array()
                        || p.is_alternate()
                        || p.is_ordered() != *ordered),
                    "array form differs from requested append"
                );
                let count = meta.array_len(namespace, path);
                ensure!(count < MAX_ITEMS, "array edit limit exceeded");
                meta.append_array_item(
                    namespace,
                    &XmpValue::new(path.clone())
                        .set_is_array(true)
                        .set_is_ordered(*ordered),
                    &XmpValue::new(value.clone()),
                )?;
                let mut check = parse(&serialize(&meta)?)?;
                ensure!(
                    check.array_len(namespace, path) == count + 1
                        && check
                            .array_item(namespace, path, (count + 1) as i32)
                            .is_some_and(|v| v.value == *value),
                    "array append did not round-trip"
                );
                if existing.is_some() {
                    check.delete_array_item(namespace, path, (count + 1) as i32)?;
                } else {
                    check.delete_property(namespace, path)?;
                }
                ensure!(
                    canonical(&check)? == original,
                    "append would alter unrelated metadata"
                );
            }
            Edit::Localized {
                namespace,
                path,
                language,
                value,
            } => {
                address(namespace, path)?;
                ensure!(
                    !language.is_empty()
                        && language
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                    "invalid metadata language"
                );
                ensure!(!value.contains('\0'), "NUL in metadata value");
                let selector = XmpMeta::compose_lang_selector(namespace, path, language)?;
                if meta.property(namespace, &selector).is_some() {
                    set_scalar(&mut meta, namespace, &selector, value)?;
                    continue;
                }
                let original = canonical(&meta)?;
                let existing = meta.property(namespace, path);
                ensure!(
                    !existing.as_ref().is_some_and(|p| !p.is_alt_text()),
                    "localized edit needs an alt-text array"
                );
                let count = meta.array_len(namespace, path);
                ensure!(count < MAX_ITEMS, "language edit limit exceeded");
                if existing.is_none() {
                    // Creates the required x-default item. A second explicit language is added below.
                    meta.set_localized_text(namespace, path, None, "x-default", value)?;
                }
                if language != "x-default" || existing.is_some() {
                    meta.append_array_item(
                        namespace,
                        &XmpValue::new(path.clone())
                            .set_is_array(true)
                            .set_is_ordered(true)
                            .set_is_alternate(true)
                            .set_is_alt_text(true),
                        &XmpValue::new(value.clone()),
                    )?;
                    let index = meta.array_len(namespace, path);
                    meta.set_qualifier(
                        namespace,
                        &format!("{path}[{index}]"),
                        XML,
                        "lang",
                        &XmpValue::new(language.clone()),
                    )?;
                }
                let mut check = parse(&serialize(&meta)?)?;
                ensure!(
                    check
                        .property(namespace, &selector)
                        .is_some_and(|v| v.value == *value),
                    "localized edit did not round-trip"
                );
                if existing.is_some() {
                    check.delete_property(namespace, &selector)?;
                } else {
                    check.delete_property(namespace, path)?;
                }
                ensure!(
                    canonical(&check)? == original,
                    "localized edit would alter other languages or qualifiers"
                );
            }
        }
    }
    let output = serialize(&meta)?;
    ensure!(
        canonical(&parse(&output)?)? == canonical(&meta)?,
        "XMP export model did not round-trip"
    );
    Ok(output)
}

pub fn empty_packet() -> Result<Vec<u8>> {
    serialize(&XmpMeta::new()?)
}

pub(crate) fn field_address(field: &str) -> Option<(&'static str, &'static str)> {
    Some(match field {
        "rating" => (XMP, "Rating"),
        "label" => (XMP, "Label"),
        "title" => (DC, "title"),
        "description" => (DC, "description"),
        "rights" => (DC, "rights"),
        "creator" => (DC, "creator"),
        "keywords" => (DC, "subject"),
        "hierarchical_keywords" => ("http://ns.adobe.com/lightroom/1.0/", "hierarchicalSubject"),
        "capture_date" => ("http://ns.adobe.com/exif/1.0/", "DateTimeOriginal"),
        "camera_make" => ("http://ns.adobe.com/tiff/1.0/", "Make"),
        "camera_model" => ("http://ns.adobe.com/tiff/1.0/", "Model"),
        "orientation" => ("http://ns.adobe.com/tiff/1.0/", "Orientation"),
        "lens" => ("http://ns.adobe.com/exif/1.0/aux/", "Lens"),
        "gps_latitude" => ("http://ns.adobe.com/exif/1.0/", "GPSLatitude"),
        "gps_longitude" => ("http://ns.adobe.com/exif/1.0/", "GPSLongitude"),
        "create_date" => (XMP, "CreateDate"),
        "modify_date" => (XMP, "ModifyDate"),
        _ => return None,
    })
}
fn xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;")
        .replace('\r', "&#13;")
        .replace('\n', "&#10;")
        .replace('\t', "&#9;")
}
type Properties = BTreeMap<(String, String), String>;
fn properties(bytes: &[u8]) -> Result<(String, Properties)> {
    let meta = parse(bytes)?;
    let name = meta.name();
    let serialized = canonical(&meta)?;
    let doc = roxmltree::Document::parse(&serialized)?;
    let rdf = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
    let mut output = BTreeMap::new();
    for description in doc.descendants().filter(|n| {
        n.has_tag_name((rdf, "Description"))
            && n.parent().is_some_and(|p| p.has_tag_name((rdf, "RDF")))
    }) {
        for property in description.children().filter(|n| n.is_element()) {
            let ns = property
                .tag_name()
                .namespace()
                .context("canonical property lacks namespace")?;
            let key = (ns.to_owned(), property.tag_name().name().to_owned());
            let mut fragment = format!("<rdf:Description rdf:about=\"{}\"", xml_attribute(&name));
            for ns in property.namespaces() {
                if let Some(prefix) = ns.name() {
                    fragment.push_str(&format!(" xmlns:{prefix}=\"{}\"", xml_attribute(ns.uri())));
                } else {
                    fragment.push_str(&format!(" xmlns=\"{}\"", xml_attribute(ns.uri())));
                }
            }
            fragment.push('>');
            fragment.push_str(&serialized[property.range()]);
            fragment.push_str("</rdf:Description>");
            ensure!(
                output.insert(key, fragment).is_none(),
                "duplicate canonical top-level property"
            );
        }
    }
    Ok((name, output))
}
fn assemble(properties: &Properties, subject: &str) -> Result<Vec<u8>> {
    let mut text = String::from(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">",
    );
    for fragment in properties.values() {
        text.push_str(fragment);
    }
    if properties.is_empty() {
        text.push_str(&format!(
            "<rdf:Description rdf:about=\"{}\"/>",
            xml_attribute(subject)
        ));
    }
    text.push_str("</rdf:RDF></x:xmpmeta>");
    serialize(&parse(text.as_bytes())?)
}
/// Copy selected complete common properties, including nested data and qualifiers.
/// A None source is an explicit removal. Everything outside these properties remains from base.
pub fn reconcile_fields(base: &[u8], fields: &[(String, Option<Vec<u8>>)]) -> Result<Vec<u8>> {
    let (subject, mut props) = properties(base)?;
    for (field, source) in fields {
        let (ns, path) = field_address(field).context("unknown indexed metadata field")?;
        let key = (ns.to_owned(), path.to_owned());
        if let Some(source) = source {
            let (incoming_subject, incoming) = properties(source)?;
            ensure!(
                incoming_subject == subject,
                "selected source RDF subject differs from base"
            );
            props.insert(
                key.clone(),
                incoming
                    .get(&key)
                    .context("selected source does not contain field")?
                    .clone(),
            );
        } else {
            props.remove(&key);
        }
    }
    let output = assemble(&props, &subject)?;
    ensure!(
        parse(&output)?.name() == subject,
        "reconciled export changed RDF subject"
    );
    let (_, actual) = properties(&output)?;
    ensure!(
        props.len() == actual.len(),
        "reconciled export lost properties"
    );
    // Compare each full property model after namespace normalization, including unknown qualifiers.
    for (key, expected) in props {
        let actual = actual
            .get(&key)
            .context("reconciled export lost a property")?;
        let expected = BTreeMap::from([(key.clone(), expected)]);
        let actual = BTreeMap::from([(key, actual.clone())]);
        ensure!(
            canonical(&parse(&assemble(&expected, &subject)?)?)?
                == canonical(&parse(&assemble(&actual, &subject)?)?)?,
            "reconciled export changed property semantics"
        );
    }
    Ok(output)
}

pub(crate) fn merge_jpeg(main: &[u8], extended: &[u8]) -> Result<Vec<u8>> {
    let (name, mut props) = properties(main)?;
    let (other, more) = properties(extended)?;
    ensure!(name == other, "JPEG main/extended RDF subjects differ");
    props.remove(&(
        "http://ns.adobe.com/xmp/note/".into(),
        "HasExtendedXMP".into(),
    ));
    for (key, value) in more {
        ensure!(
            props.insert(key, value).is_none(),
            "JPEG main/extended properties overlap; explicit reconciliation required"
        );
    }
    assemble(&props, &name)
}

/// Prefix-independent full-property identities for conflict detection. Values alone
/// cannot distinguish two ratings or keywords carrying different unknown qualifiers.
pub(crate) fn field_semantics(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    fn node_value(node: roxmltree::Node<'_, '_>) -> serde_json::Value {
        if node.is_text() {
            return serde_json::json!(["text", node.text().unwrap_or_default()]);
        }
        let mut attributes = node
            .attributes()
            .map(|a| (a.namespace().unwrap_or_default(), a.name(), a.value()))
            .collect::<Vec<_>>();
        attributes.sort();
        serde_json::json!([
            node.tag_name().namespace().unwrap_or_default(),
            node.tag_name().name(),
            attributes,
            node.children()
                .filter(|n| n.is_element() || n.is_text())
                .map(node_value)
                .collect::<Vec<_>>()
        ])
    }
    let meta = parse(bytes)?;
    let serialized = canonical(&meta)?;
    let doc = roxmltree::Document::parse(&serialized)?;
    let projection = project(bytes)?;
    let mut result = BTreeMap::new();
    for field in projection.fields.keys() {
        let address = field_address(field).context("unknown projected field")?;
        let node = doc
            .descendants()
            .find(|n| {
                n.has_tag_name(address)
                    && n.parent().is_some_and(|p| {
                        p.has_tag_name((
                            "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
                            "Description",
                        )) && p.parent().is_some_and(|r| {
                            r.has_tag_name(("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "RDF"))
                        })
                    })
            })
            .context("canonical projected property missing")?;
        result.insert(
            field.clone(),
            blake3::hash(&serde_json::to_vec(&node_value(node))?)
                .to_hex()
                .to_string(),
        );
    }
    Ok(result)
}
