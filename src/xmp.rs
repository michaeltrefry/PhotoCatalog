//! Fallible XMP interpretation and explicit edits. Original packet bytes live separately.
mod semantics_json;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use xmp_toolkit::{AdmittedString, FromStrOptions, ToStringOptions, XmpMeta, XmpValue};

use crate::lightroom_migration_worker::memory::requested::{Requested, Scope};

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
fn encoding(bytes: &[u8]) -> (usize, usize, bool) {
    if bytes.starts_with(&[0xff, 0xfe, 0, 0]) {
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
    }
}

pub(crate) fn decoded_text_bytes(bytes: &[u8]) -> Result<usize> {
    ensure!(
        bytes.len() <= MAX_PACKET_BYTES,
        "XMP packet exceeds parse limit"
    );
    let (skip, width, little) = encoding(bytes);
    let body = &bytes[skip..];
    match width {
        1 => Ok(std::str::from_utf8(body).context("XMP is not UTF-8")?.len()),
        2 => {
            ensure!(body.len().is_multiple_of(2), "truncated UTF-16 XMP");
            std::char::decode_utf16(body.as_chunks::<2>().0.iter().map(|b| {
                if little {
                    u16::from_le_bytes([b[0], b[1]])
                } else {
                    u16::from_be_bytes([b[0], b[1]])
                }
            }))
            .try_fold(0usize, |length, character| {
                length
                    .checked_add(character.context("invalid UTF-16 XMP")?.len_utf8())
                    .context("decoded XMP byte length overflow")
            })
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
                .try_fold(0usize, |length, character| {
                    length
                        .checked_add(character?.len_utf8())
                        .context("decoded XMP byte length overflow")
                })
        }
        _ => unreachable!(),
    }
}

/// Maximum valid UTF-8 length for this packet's detected transport encoding.
/// UTF-16 needs at most three UTF-8 bytes per two input bytes; UTF-8 and UTF-32
/// cannot expand beyond their original byte count. This does not validate the
/// packet and therefore cannot turn a later observed parse error into a failure.
pub(crate) fn decoded_text_bound(bytes: &[u8]) -> Result<usize> {
    let (skip, width, _) = encoding(bytes);
    let body = bytes.len().saturating_sub(skip);
    if width == 2 {
        crate::lightroom_migration_worker::memory::layout::mul(body.div_ceil(2), 3)
    } else {
        Ok(body)
    }
}

/// Complete persistent Projection plus field-semantics graph for one model.
/// The projection has seventeen fixed field names, three simple arrays and
/// three localized maps, each using the existing 10,000-item acceptance. All
/// returned value strings are present in the already bounded compact packet,
/// so their combined payload is bounded by MAX_PACKET_BYTES.
pub(crate) fn prepared_model_storage() -> Result<usize> {
    use crate::lightroom_migration_worker::memory::layout::{add, mul, tree, vector};
    const FIELDS: [&str; 17] = [
        "rating",
        "label",
        "capture_date",
        "camera_make",
        "camera_model",
        "orientation",
        "lens",
        "gps_latitude",
        "gps_longitude",
        "create_date",
        "modify_date",
        "title",
        "description",
        "rights",
        "creator",
        "keywords",
        "hierarchical_keywords",
    ];
    let field_names = FIELDS
        .iter()
        .try_fold(0usize, |bytes, field| add(bytes, field.len()))?;
    let issues = FIELDS.iter().try_fold(0usize, |bytes, field| {
        let scalar = add(
            field.len(),
            ": expected scalar; original structure retained".len(),
        )?;
        let array = add(
            field.len(),
            ": non-simple or ambiguous items; full array retained".len(),
        )?;
        let limit = add(
            field.len(),
            ": projection limit exceeded; full array retained".len(),
        )?;
        add(bytes, scalar.max(array).max(limit))
    })?;
    let projection_containers = add(
        tree::<String, Value>(FIELDS.len())?,
        add(
            mul(3, vector::<String>(MAX_ITEMS)?)?,
            mul(3, tree::<String, String>(MAX_ITEMS)?)?,
        )?,
    )?;
    let semantics = add(
        tree::<String, String>(FIELDS.len())?,
        add(field_names, mul(64, FIELDS.len())?)?,
    )?;
    add(
        MAX_PACKET_BYTES,
        add(
            projection_containers,
            add(
                semantics,
                add(vector::<String>(FIELDS.len())?, add(issues, field_names)?)?,
            )?,
        )?,
    )
}

/// Strict UTF-8/16/32 decoding; retained bytes are never replaced by this parse representation.
pub fn xml_text(bytes: &[u8]) -> Result<String> {
    let length = decoded_text_bytes(bytes)?;
    let (skip, width, little) = encoding(bytes);
    let body = &bytes[skip..];
    let mut text = String::with_capacity(length);
    match width {
        1 => text.push_str(std::str::from_utf8(body).context("XMP is not UTF-8")?),
        2 => {
            for character in std::char::decode_utf16(body.as_chunks::<2>().0.iter().map(|b| {
                if little {
                    u16::from_le_bytes([b[0], b[1]])
                } else {
                    u16::from_be_bytes([b[0], b[1]])
                }
            })) {
                text.push(character.context("invalid UTF-16 XMP")?);
            }
        }
        4 => {
            for b in body.as_chunks::<4>().0 {
                let value = [b[0], b[1], b[2], b[3]];
                text.push(
                    char::from_u32(if little {
                        u32::from_le_bytes(value)
                    } else {
                        u32::from_be_bytes(value)
                    })
                    .context("invalid UTF-32 XMP")?,
                );
            }
        }
        _ => unreachable!(),
    }
    debug_assert_eq!(text.len(), length);
    ensure!(!text.contains('\0'), "NUL in XMP text");
    ensure!(
        !text.contains("<!DOCTYPE") && !text.contains("<!ENTITY"),
        "DTD/entity declarations are not accepted in XMP"
    );
    Ok(text)
}
pub fn parse(bytes: &[u8]) -> Result<XmpMeta> {
    let admit = |_| Ok(());
    let requested = Requested::new(&admit);
    parse_admitted(bytes, &requested)
}

pub(crate) fn parse_admitted<'a>(bytes: &[u8], requested: &'a Requested<'a>) -> Result<XmpMeta> {
    let _text_scope = requested.scope(decoded_text_bytes(bytes)?)?;
    let text = xml_text(bytes)?;
    let _document_scope =
        requested.scope(crate::lightroom_migration_worker::memory::core::xml_document(&text)?)?;
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
    let mut roots = document
        .descendants()
        .filter(|n| n.has_tag_name((RDF, "RDF")));
    ensure!(
        roots.next().is_some() && roots.next().is_none(),
        "XMP must contain exactly one RDF model"
    );
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
    // Compact output has an existing 16 MiB pre-copy bound. Its native C
    // backing and the SDK model are separate native owners; this scope covers
    // only the bounded Rust copy returned by the wrapper.
    let _compact_scope = requested.scope(MAX_PACKET_BYTES)?;
    let serialized = serialize(&model)?;
    crate::xmp_rdf::assert_equivalent_admitted(
        &text,
        std::str::from_utf8(&serialized)?,
        requested,
    )?;
    Ok(model)
}
fn serialize(meta: &XmpMeta) -> Result<Vec<u8>> {
    Ok(meta
        .to_string_with_options_bounded(
            ToStringOptions::default().use_compact_format(),
            MAX_PACKET_BYTES,
        )?
        .context("edited XMP exceeds limit")?
        .into_bytes())
}
/// Full fallible serialization avoids the SDK wrapper's lossy Clone/iterator error paths.
pub fn canonical(meta: &XmpMeta) -> Result<String> {
    let admit = |_| Ok(());
    let requested = Requested::new(&admit);
    Ok(canonical_admitted(meta, &requested)?.as_ref().to_owned())
}

pub(crate) fn canonical_admitted<'a>(
    meta: &XmpMeta,
    requested: &'a Requested<'a>,
) -> Result<AdmittedString<Scope<'a>>> {
    let _compact_scope = requested.scope(MAX_PACKET_BYTES)?;
    let serialized = serialize(meta)?;
    let mut copy = parse_admitted(&serialized, requested)?;
    copy.sort()?;
    copy.to_string_with_options_admitted(
        ToStringOptions::default()
            .omit_packet_wrapper()
            .use_canonical_format()
            .omit_all_formatting(),
        |bytes| requested.scope(bytes),
    )
}

pub fn project(bytes: &[u8]) -> Result<Projection> {
    let admit = |_| Ok(());
    let requested = Requested::new(&admit);
    project_admitted(bytes, &requested)
}

pub(crate) fn project_admitted<'a>(
    bytes: &[u8],
    requested: &'a Requested<'a>,
) -> Result<Projection> {
    let meta = parse_admitted(bytes, requested)?;
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
    apply_edits_inner(bytes, edits)
}
/// Organization may update each of the already bounded 10,000 array items. Keep
/// one native mutation pass: sorting between chunks would change item addresses.
pub(crate) fn apply_organization_edits(bytes: &[u8], edits: &[Edit]) -> Result<Vec<u8>> {
    ensure!(
        edits.len() <= MAX_ITEMS,
        "too many organization array edits"
    );
    if !edits.is_empty()
        && edits.iter().all(|edit| match edit {
            Edit::Set {
                namespace, path, ..
            } => {
                namespace == "http://ns.adobe.com/lightroom/1.0/"
                    && path
                        .strip_prefix("hierarchicalSubject[")
                        .and_then(|p| p.strip_suffix(']'))
                        .is_some_and(|p| p.parse::<usize>().is_ok_and(|i| i > 0))
            }
            _ => false,
        })
    {
        return replace_hierarchy_items(bytes, edits);
    }
    apply_edits_inner(bytes, edits)
}
// One full-model undo comparison proves all untouched properties and qualifiers
// survive a batch of disjoint existing scalar replacements. Do not canonicalize
// between writes: that would reorder RDF Bags and invalidate the item addresses.
fn replace_hierarchy_items(bytes: &[u8], edits: &[Edit]) -> Result<Vec<u8>> {
    let mut meta = parse(bytes)?;
    let before = canonical(&meta)?;
    let mut originals = Vec::with_capacity(edits.len());
    let mut paths = std::collections::BTreeSet::new();
    for edit in edits {
        let Edit::Set {
            namespace,
            path,
            value,
        } = edit
        else {
            unreachable!()
        };
        address(namespace, path)?;
        ensure!(
            paths.insert(path),
            "duplicate hierarchy replacement address"
        );
        ensure!(!value.contains('\0'), "NUL in metadata value");
        let old = meta
            .property(namespace, path)
            .context("hierarchy replacement item absent")?;
        ensure!(
            !old.is_array() && !old.is_struct(),
            "hierarchy replacement item is structured"
        );
        meta.set_property(
            namespace,
            path,
            &XmpValue::new(value.clone()).set_is_uri(old.is_uri()),
        )?;
        originals.push((namespace, path, value, old));
    }
    let output = serialize(&meta)?;
    let mut check = parse(&output)?;
    for (ns, path, value, _) in &originals {
        ensure!(
            check.property(ns, path).is_some_and(|p| p.value == **value),
            "hierarchy replacement did not round-trip"
        );
    }
    for (ns, path, _, old) in originals {
        check.set_property(
            ns,
            path,
            &XmpValue::new(old.value.clone()).set_is_uri(old.is_uri()),
        )?;
    }
    ensure!(
        canonical(&check)? == before,
        "hierarchy replacement changed unrelated XMP semantics"
    );
    Ok(output)
}
fn apply_edits_inner(bytes: &[u8], edits: &[Edit]) -> Result<Vec<u8>> {
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
fn push_xml_attribute(output: &mut String, value: &str) {
    for character in value.chars() {
        output.push_str(match character {
            '&' => "&amp;",
            '<' => "&lt;",
            '"' => "&quot;",
            '\r' => "&#13;",
            '\n' => "&#10;",
            '\t' => "&#9;",
            _ => {
                output.push(character);
                continue;
            }
        });
    }
}
type Properties = BTreeMap<(String, String), String>;
struct AdmittedProperties<'a> {
    name: String,
    properties: Properties,
    // Owner fields drop before their requested-storage guard.
    _scope: Scope<'a>,
}
fn properties(bytes: &[u8]) -> Result<(String, Properties)> {
    let admit = |_| Ok(());
    let requested = Requested::new(&admit);
    let admitted = properties_admitted(bytes, &requested)?;
    Ok((admitted.name, admitted.properties))
}

fn escaped_attribute_bytes(value: &str) -> Result<usize> {
    value.chars().try_fold(0usize, |bytes, character| {
        bytes
            .checked_add(match character {
                '&' => 5,
                '<' => 4,
                '"' => 6,
                '\r' => 5,
                '\n' | '\t' => 4,
                _ => character.len_utf8(),
            })
            .context("escaped XML attribute byte length overflow")
    })
}

fn properties_admitted<'a>(
    bytes: &[u8],
    requested: &'a Requested<'a>,
) -> Result<AdmittedProperties<'a>> {
    let meta = parse_admitted(bytes, requested)?;
    let serialized = canonical_admitted(&meta, requested)?;
    let _document_scope = requested
        .scope(crate::lightroom_migration_worker::memory::core::xml_document(&serialized)?)?;
    let doc = roxmltree::Document::parse(&serialized)?;
    let rdf = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
    let descriptions = doc.descendants().filter(|node| {
        node.has_tag_name((rdf, "Description"))
            && node
                .parent()
                .is_some_and(|parent| parent.has_tag_name((rdf, "RDF")))
    });
    let mut count = 0usize;
    let mut owned = 0usize;
    for description in descriptions {
        let subject = description.attribute((rdf, "about")).unwrap_or_default();
        for property in description.children().filter(|node| node.is_element()) {
            count = count
                .checked_add(1)
                .context("canonical property count overflow")?;
            let namespace = property
                .tag_name()
                .namespace()
                .context("canonical property lacks namespace")?;
            owned = owned
                .checked_add(namespace.len())
                .and_then(|value| value.checked_add(property.tag_name().name().len()))
                .and_then(|value| value.checked_add("<rdf:Description rdf:about=\"\"".len()))
                .context("canonical property storage overflow")?;
            owned = owned
                .checked_add(escaped_attribute_bytes(subject)?)
                .and_then(|value| value.checked_add(1 + property.range().len()))
                .and_then(|value| value.checked_add("</rdf:Description>".len()))
                .context("canonical property storage overflow")?;
            for namespace in property.namespaces() {
                let prefix = namespace.name().map_or(0, str::len);
                owned = owned
                    .checked_add(if namespace.name().is_some() {
                        " xmlns:=\"\"".len() + prefix
                    } else {
                        " xmlns=\"\"".len()
                    })
                    .context("canonical namespace storage overflow")?;
                owned = owned
                    .checked_add(escaped_attribute_bytes(namespace.uri())?)
                    .context("canonical namespace storage overflow")?;
            }
        }
    }
    let name_bytes = doc
        .descendants()
        .find(|node| node.has_tag_name((rdf, "Description")))
        .and_then(|node| node.attribute((rdf, "about")))
        .unwrap_or_default()
        .len();
    let map =
        crate::lightroom_migration_worker::memory::layout::tree::<(String, String), String>(count)?;
    let property_scope = requested.scope(
        owned
            .checked_add(name_bytes)
            .and_then(|value| value.checked_add(map))
            .context("canonical property graph overflow")?,
    )?;
    let name = meta.name();
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
            let mut fragment_bytes = "<rdf:Description rdf:about=\"\"".len()
                + escaped_attribute_bytes(&name)?
                + 1
                + property.range().len()
                + "</rdf:Description>".len();
            for namespace in property.namespaces() {
                fragment_bytes = fragment_bytes
                    .checked_add(if let Some(prefix) = namespace.name() {
                        " xmlns:=\"\"".len() + prefix.len()
                    } else {
                        " xmlns=\"\"".len()
                    })
                    .context("canonical property fragment overflow")?;
                fragment_bytes = fragment_bytes
                    .checked_add(escaped_attribute_bytes(namespace.uri())?)
                    .context("canonical property fragment overflow")?;
            }
            let mut fragment = String::with_capacity(fragment_bytes);
            fragment.push_str("<rdf:Description rdf:about=\"");
            push_xml_attribute(&mut fragment, &name);
            fragment.push('"');
            for ns in property.namespaces() {
                if let Some(prefix) = ns.name() {
                    fragment.push_str(" xmlns:");
                    fragment.push_str(prefix);
                    fragment.push_str("=\"");
                } else {
                    fragment.push_str(" xmlns=\"");
                }
                push_xml_attribute(&mut fragment, ns.uri());
                fragment.push('"');
            }
            fragment.push('>');
            fragment.push_str(&serialized[property.range()]);
            fragment.push_str("</rdf:Description>");
            debug_assert_eq!(fragment.len(), fragment_bytes);
            ensure!(
                output.insert(key, fragment).is_none(),
                "duplicate canonical top-level property"
            );
        }
    }
    Ok(AdmittedProperties {
        name,
        properties: output,
        _scope: property_scope,
    })
}
fn assemble(properties: &Properties, subject: &str) -> Result<Vec<u8>> {
    let admit = |_| Ok(());
    let requested = Requested::new(&admit);
    Ok(assemble_admitted(properties, subject, &requested)?.0)
}

fn assemble_admitted<'a>(
    properties: &Properties,
    subject: &str,
    requested: &'a Requested<'a>,
) -> Result<(Vec<u8>, Scope<'a>)> {
    const OPEN: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">";
    const CLOSE: &str = "</rdf:RDF></x:xmpmeta>";
    let fragments = properties.values().try_fold(0usize, |bytes, fragment| {
        bytes
            .checked_add(fragment.len())
            .context("assembled XMP byte length overflow")
    })?;
    let empty = if properties.is_empty() {
        "<rdf:Description rdf:about=\"\"/>".len() + escaped_attribute_bytes(subject)?
    } else {
        0
    };
    let text_bytes = OPEN
        .len()
        .checked_add(fragments)
        .and_then(|value| value.checked_add(empty))
        .and_then(|value| value.checked_add(CLOSE.len()))
        .context("assembled XMP byte length overflow")?;
    let _text_scope = requested.scope(text_bytes)?;
    let mut text = String::with_capacity(text_bytes);
    text.push_str(OPEN);
    for fragment in properties.values() {
        text.push_str(fragment);
    }
    if properties.is_empty() {
        text.push_str("<rdf:Description rdf:about=\"");
        push_xml_attribute(&mut text, subject);
        text.push_str("\"/>");
    }
    text.push_str(CLOSE);
    debug_assert_eq!(text.len(), text_bytes);
    let meta = parse_admitted(text.as_bytes(), requested)?;
    let output_scope = requested.scope(MAX_PACKET_BYTES)?;
    Ok((serialize(&meta)?, output_scope))
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

/// Technical metadata for physically rendered pixels. This explicit derivative
/// policy removes active Adobe development instructions, old thumbnails and
/// source encoding declarations. Retained source packets are never modified.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DerivativeFields {
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
    pub mime_type: String,
    pub profile_name: String,
    pub is_srgb: bool,
}

pub fn rendered_derivative(base: &[u8], fields: &DerivativeFields) -> Result<Vec<u8>> {
    const TIFF: &str = "http://ns.adobe.com/tiff/1.0/";
    const EXIF: &str = "http://ns.adobe.com/exif/1.0/";
    const PHOTOSHOP: &str = "http://ns.adobe.com/photoshop/1.0/";
    const CRS: &str = "http://ns.adobe.com/camera-raw-settings/1.0/";
    ensure!(
        fields.width > 0
            && fields.height > 0
            && (3..=4).contains(&fields.channels)
            && [8, 16, 32].contains(&fields.bits_per_sample),
        "invalid derivative dimensions/depth"
    );
    ensure!(
        ["image/jpeg", "image/png", "image/tiff"].contains(&fields.mime_type.as_str()),
        "unsupported derivative MIME type"
    );
    ensure!(
        !fields.profile_name.is_empty() && fields.profile_name.len() <= 1024,
        "derivative profile name limit"
    );
    let (subject, mut props) = properties(base)?;
    let replaced = |namespace: &str, name: &str| -> bool {
        namespace == CRS
            || (namespace == TIFF
                && [
                    "ImageWidth",
                    "ImageLength",
                    "Orientation",
                    "BitsPerSample",
                    "SamplesPerPixel",
                    "PhotometricInterpretation",
                    "Compression",
                    "PlanarConfiguration",
                    "SampleFormat",
                    "ExtraSamples",
                    "StripOffsets",
                    "StripByteCounts",
                    "RowsPerStrip",
                    "TileOffsets",
                    "TileByteCounts",
                    "TileWidth",
                    "TileLength",
                    "JPEGInterchangeFormat",
                    "JPEGInterchangeFormatLength",
                ]
                .contains(&name))
            || (namespace == EXIF
                && [
                    "PixelXDimension",
                    "PixelYDimension",
                    "ColorSpace",
                    "ComponentsConfiguration",
                    "CompressedBitsPerPixel",
                    "MakerNote",
                ]
                .contains(&name))
            || (namespace == PHOTOSHOP && name == "ICCProfile")
            || (namespace == DC && name == "format")
            || (namespace == XMP && ["Thumbnails", "CreatorTool"].contains(&name))
            || (namespace == "http://ns.adobe.com/xmp/note/" && name == "HasExtendedXMP")
    };
    props.retain(|(namespace, name), _| !replaced(namespace, name));
    let preserved = props.clone();
    let mut edits = Vec::new();
    for (namespace, path, value) in [
        (TIFF, "ImageWidth", fields.width.to_string()),
        (TIFF, "ImageLength", fields.height.to_string()),
        (TIFF, "Orientation", "1".into()),
        (TIFF, "SamplesPerPixel", fields.channels.to_string()),
        (TIFF, "PhotometricInterpretation", "2".into()),
        (EXIF, "PixelXDimension", fields.width.to_string()),
        (EXIF, "PixelYDimension", fields.height.to_string()),
        (
            EXIF,
            "ColorSpace",
            if fields.is_srgb { "1" } else { "65535" }.into(),
        ),
        (PHOTOSHOP, "ICCProfile", fields.profile_name.clone()),
        (DC, "format", fields.mime_type.clone()),
        (XMP, "CreatorTool", "LensWorks".into()),
    ] {
        edits.push(Edit::Set {
            namespace: namespace.into(),
            path: path.into(),
            value,
        });
    }
    for _ in 0..fields.channels {
        edits.push(Edit::Append {
            namespace: TIFF.into(),
            path: "BitsPerSample".into(),
            value: fields.bits_per_sample.to_string(),
            ordered: true,
        });
    }
    let mut technical_base = parse(&empty_packet()?)?;
    technical_base.set_name(&subject)?;
    let technical = apply_edits(&serialize(&technical_base)?, &edits)?;
    let (_, new) = properties(&technical)?;
    for (key, fragment) in new {
        props.insert(key, fragment);
    }
    let output = assemble(&props, &subject)?;
    let (actual_subject, actual) = properties(&output)?;
    ensure!(
        actual_subject == subject && actual.len() == props.len(),
        "derivative changed RDF subject or property count"
    );
    for (key, expected) in preserved {
        let observed = actual
            .get(&key)
            .context("derivative lost unrelated metadata")?;
        let before = BTreeMap::from([(key.clone(), expected)]);
        let after = BTreeMap::from([(key, observed.clone())]);
        ensure!(
            canonical(&parse(&assemble(&before, &subject)?)?)?
                == canonical(&parse(&assemble(&after, &subject)?)?)?,
            "derivative changed unrelated XMP semantics"
        );
    }
    Ok(output)
}

#[cfg(test)]
pub(crate) fn merge_jpeg(main: &[u8], extended: &[u8]) -> Result<Vec<u8>> {
    let admit = |_| Ok(());
    let requested = Requested::new(&admit);
    Ok(merge_jpeg_admitted(main, extended, &requested)?.0)
}

pub(crate) fn merge_jpeg_admitted<'a>(
    main: &[u8],
    extended: &[u8],
    requested: &'a Requested<'a>,
) -> Result<(Vec<u8>, Scope<'a>)> {
    let mut main = properties_admitted(main, requested)?;
    let extended = properties_admitted(extended, requested)?;
    ensure!(
        main.name == extended.name,
        "JPEG main/extended RDF subjects differ"
    );
    main.properties.remove(&(
        "http://ns.adobe.com/xmp/note/".into(),
        "HasExtendedXMP".into(),
    ));
    for (key, value) in extended.properties {
        ensure!(
            main.properties.insert(key, value).is_none(),
            "JPEG main/extended properties overlap; explicit reconciliation required"
        );
    }
    assemble_admitted(&main.properties, &main.name, requested)
}

/// Prefix-independent full-property identities for conflict detection. Values alone
/// cannot distinguish two ratings or keywords carrying different unknown qualifiers.
pub(crate) fn field_semantics(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let admit = |_| Ok(());
    let requested = Requested::new(&admit);
    field_semantics_admitted(bytes, &requested)
}

pub(crate) fn field_semantics_admitted<'a>(
    bytes: &[u8],
    requested: &'a Requested<'a>,
) -> Result<BTreeMap<String, String>> {
    let meta = parse_admitted(bytes, requested)?;
    let serialized = canonical_admitted(&meta, requested)?;
    let _document_scope = requested
        .scope(crate::lightroom_migration_worker::memory::core::xml_document(&serialized)?)?;
    let doc = roxmltree::Document::parse(&serialized)?;
    let projection = project_admitted(bytes, requested)?;
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
        let _attributes_scope = requested.scope(semantics_json::attribute_bytes(node)?)?;
        result.insert(field.clone(), semantics_json::hash(node)?);
    }
    Ok(result)
}
