//! Independent, bounded RDF comparison at the native XMP interpretation boundary.
//! This models XMP's RDF forms, not arbitrary RDF graphs. Unsupported identity or
//! datatype constructs fail explicitly rather than being ignored by comparison.
use anyhow::{Context, Result, bail, ensure};
use roxmltree::{Document, Node};
use std::collections::BTreeMap;

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const XML: &str = "http://www.w3.org/XML/1998/namespace";
const MAX_BYTES: usize = 16 * 1024 * 1024;
type Name = (String, String);
type Properties = BTreeMap<Name, Value>;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Value {
    Literal(String),
    Uri(String),
    Struct(Properties),
    Array(String, Vec<Value>),
    Qualified(Box<Value>, Properties),
}

#[derive(Debug, PartialEq, Eq)]
struct Model {
    subject: String,
    properties: Properties,
}

/// Compare decoded XML independently of the Adobe SDK. Prefixes, compact RDF
/// attributes, and equivalent struct/qualifier encodings do not affect equality.
/// Bag membership and valid language alternatives are unordered. Language tags
/// compare ASCII case-insensitively; duplicate normalized alternatives are errors.
/// Sequence order,
/// duplicates, literal whitespace, unknown qualifiers, and subjects are retained.
pub fn assert_equivalent(source_xml: &str, serialized_xml: &str) -> Result<()> {
    let source = model(source_xml).context("original RDF semantics")?;
    let serialized = model(serialized_xml).context("serialized RDF semantics")?;
    ensure!(
        source.subject == serialized.subject,
        "XMP serialization changed RDF subject"
    );
    ensure!(
        source.properties.len() == serialized.properties.len(),
        "XMP serialization changed property count"
    );
    for (name, value) in source.properties {
        ensure!(
            serialized.properties.get(&name) == Some(&value),
            "XMP serialization changed property {{{}}}{}",
            name.0,
            name.1
        );
    }
    Ok(())
}

fn name(namespace: Option<&str>, local: &str) -> Result<Name> {
    let namespace = namespace.context("RDF names require a namespace")?;
    Ok((namespace.to_owned(), local.to_owned()))
}

fn insert(properties: &mut Properties, key: Name, value: Value) -> Result<()> {
    ensure!(
        !properties.contains_key(&key),
        "duplicate RDF property or qualifier {{{}}}{}",
        key.0,
        key.1
    );
    properties.insert(key, value);
    Ok(())
}

fn model(text: &str) -> Result<Model> {
    ensure!(text.len() <= MAX_BYTES, "RDF comparison exceeds byte limit");
    let doc = Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 100_000,
            ..Default::default()
        },
    )?;
    for node in doc.descendants().filter(Node::is_element) {
        ensure!(
            node.ancestors().take(66).count() <= 65,
            "RDF comparison exceeds nesting limit"
        );
        for a in node.attributes() {
            ensure!(
                !(a.namespace() == Some(RDF) && matches!(a.name(), "ID" | "nodeID" | "datatype")),
                "unsupported RDF identity/datatype attribute: {}",
                a.name()
            );
        }
    }
    let mut roots = doc.descendants().filter(|n| n.has_tag_name((RDF, "RDF")));
    let root = roots.next().context("missing RDF model")?;
    ensure!(roots.next().is_none(), "multiple RDF models");
    for ancestor in root.ancestors().skip(1) {
        ensure!(
            ancestor.attribute((XML, "base")).is_none()
                && ancestor.attribute((XML, "lang")).is_none(),
            "unsupported inherited XML base/language on RDF model"
        );
    }
    ensure!(
        root.attributes().len() == 0,
        "unsupported attributes on rdf:RDF"
    );
    let mut subject = None;
    let mut properties = Properties::new();
    for description in elements(root)? {
        ensure!(
            description.has_tag_name((RDF, "Description")),
            "unsupported top-level RDF node"
        );
        let current = description.attribute((RDF, "about")).unwrap_or_default();
        if let Some(previous) = subject {
            ensure!(previous == current, "multiple RDF subjects");
        } else {
            subject = Some(current);
        }
        let fields = fields(description, true)?;
        for (key, value) in fields {
            insert(&mut properties, key, value)?;
        }
    }
    Ok(Model {
        subject: subject.unwrap_or_default().to_owned(),
        properties,
    })
}

fn elements<'a, 'input>(node: Node<'a, 'input>) -> Result<Vec<Node<'a, 'input>>> {
    ensure!(
        node.children().filter(Node::is_text).all(|n| n
            .text()
            .unwrap_or_default()
            .bytes()
            .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))),
        "mixed RDF text and element content"
    );
    Ok(node.children().filter(Node::is_element).collect())
}

// RDF Description attributes are compact fields. Attribute qualifiers on a
// property are handled separately, so they cannot become structure fields.
fn fields(node: Node<'_, '_>, top: bool) -> Result<Properties> {
    let mut output = Properties::new();
    for a in node.attributes() {
        if a.namespace() == Some(RDF) && a.name() == "about" && top {
            continue;
        }
        ensure!(
            a.namespace() != Some(RDF) && a.namespace() != Some(XML),
            "unsupported RDF structure attribute {}",
            a.name()
        );
        insert(
            &mut output,
            name(a.namespace(), a.name())?,
            Value::Literal(a.value().to_owned()),
        )?;
    }
    for child in elements(node)? {
        let tag = child.tag_name();
        ensure!(
            tag.namespace() != Some(RDF) || matches!(tag.name(), "value" | "type"),
            "unsupported RDF property {}",
            tag.name()
        );
        insert(
            &mut output,
            name(tag.namespace(), tag.name())?,
            property(child)?,
        )?;
    }
    Ok(output)
}

fn qualified(value: Value, mut qualifiers: Properties) -> Result<Value> {
    let value = if let Value::Qualified(inner, prior) = value {
        for (key, value) in prior {
            insert(&mut qualifiers, key, value)?;
        }
        *inner
    } else {
        value
    };
    if qualifiers.is_empty() {
        Ok(value)
    } else {
        Ok(Value::Qualified(Box::new(value), qualifiers))
    }
}

fn property(node: Node<'_, '_>) -> Result<Value> {
    let children = node.children().filter(Node::is_element).collect::<Vec<_>>();
    let mut attributes = Properties::new();
    let mut language = Properties::new();
    let mut resource = None;
    let mut scalar = None;
    let mut structure = false;
    for a in node.attributes() {
        match (a.namespace(), a.name()) {
            (Some(RDF), "parseType") => {
                ensure!(a.value() == "Resource", "unsupported RDF parseType");
                structure = true;
            }
            (Some(RDF), "resource") => resource = Some(a.value()),
            (Some(RDF), "value") => scalar = Some(a.value()),
            (Some(XML), "lang") => {
                insert(
                    &mut language,
                    name(Some(XML), "lang")?,
                    Value::Literal(a.value().to_ascii_lowercase()),
                )?;
            }
            (Some(RDF), _) | (Some(XML), _) => {
                bail!("unsupported RDF property attribute {}", a.name())
            }
            _ => insert(
                &mut attributes,
                name(a.namespace(), a.name())?,
                Value::Literal(a.value().to_owned()),
            )?,
        }
    }
    ensure!(
        resource.is_none() || scalar.is_none(),
        "conflicting RDF resource/value attributes"
    );
    if resource.is_some() || scalar.is_some() {
        ensure!(
            children.is_empty()
                && !structure
                && node
                    .children()
                    .filter(Node::is_text)
                    .all(|n| n.text().unwrap_or_default().is_empty()),
            "content on RDF resource/value property"
        );
        let value = if let Some(uri) = resource {
            Value::Uri(uri.to_owned())
        } else {
            Value::Literal(scalar.unwrap_or_default().to_owned())
        };
        for (key, value) in language {
            insert(&mut attributes, key, value)?;
        }
        return qualified(value, attributes);
    }
    if structure {
        ensure!(
            attributes.is_empty(),
            "unexpected fields as parseType attributes"
        );
        let mut fields = Properties::new();
        for child in elements(node)? {
            let t = child.tag_name();
            insert(
                &mut fields,
                name(t.namespace(), t.name())?,
                property(child)?,
            )?;
        }
        let value = if let Some(value) = fields.remove(&(RDF.into(), "value".into())) {
            qualified(value, fields)?
        } else {
            Value::Struct(fields)
        };
        return qualified(value, language);
    }
    if !children.is_empty() {
        elements(node)?;
        ensure!(
            attributes.is_empty() && children.len() == 1,
            "unsupported RDF resource property content"
        );
        let child = children[0];
        let tag = child.tag_name();
        let value = if tag.namespace() == Some(RDF) && matches!(tag.name(), "Bag" | "Seq" | "Alt") {
            ensure!(
                child.attributes().len() == 0,
                "unsupported RDF array attributes"
            );
            let mut items = Vec::new();
            for item in elements(child)? {
                ensure!(
                    item.has_tag_name((RDF, "li")),
                    "unsupported numbered or named RDF array member"
                );
                items.push(property(item)?);
            }
            if tag.name() == "Bag" || (tag.name() == "Alt" && language_alternatives(&items)?) {
                items.sort();
            }
            Value::Array(tag.name().to_owned(), items)
        } else {
            let mut fields = fields(child, false)?;
            let value = if let Some(value) = fields.remove(&(RDF.into(), "value".into())) {
                qualified(value, fields)?
            } else {
                Value::Struct(fields)
            };
            if child.has_tag_name((RDF, "Description")) {
                value
            } else {
                ensure!(tag.namespace() != Some(RDF), "unsupported typed RDF node");
                qualified(
                    value,
                    BTreeMap::from([(
                        (RDF.into(), "type".into()),
                        Value::Uri(format!(
                            "{}{}",
                            tag.namespace().context("typed RDF node lacks namespace")?,
                            tag.name()
                        )),
                    )]),
                )?
            }
        };
        return qualified(value, language);
    }
    let value = if !attributes.is_empty() {
        ensure!(
            node.children()
                .filter(Node::is_text)
                .all(|n| n.text().unwrap_or_default().is_empty()),
            "text on compact RDF structure"
        );
        Value::Struct(attributes)
    } else {
        Value::Literal(
            node.children()
                .filter(Node::is_text)
                .map(|n| n.text().unwrap_or_default())
                .collect(),
        )
    };
    qualified(value, language)
}

fn language_alternatives(items: &[Value]) -> Result<bool> {
    let mut languages = std::collections::BTreeSet::new();
    let mut all_languages = !items.is_empty();
    for item in items {
        if let Value::Qualified(value, qualifiers) = item
            && let Some(Value::Literal(language)) = qualifiers.get(&(XML.into(), "lang".into()))
        {
            ensure!(
                languages.insert(language),
                "ambiguous duplicate normalized RDF language alternative"
            );
            all_languages &= !language.is_empty() && matches!(**value, Value::Literal(_));
        } else {
            all_languages = false;
        }
    }
    Ok(all_languages)
}
