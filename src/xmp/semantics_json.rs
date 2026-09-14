//! Exact legacy field-semantics JSON streamed without an owned Value tree.
use anyhow::{Context, Result};
use serde::{Serialize, Serializer, ser::SerializeSeq};
use std::io::Write;

pub(super) fn attribute_bytes(node: roxmltree::Node<'_, '_>) -> Result<usize> {
    let count = node.descendants().try_fold(0usize, |n, child| {
        n.checked_add(child.attributes().len())
            .context("field-semantics attribute count overflow")
    })?;
    count
        .checked_mul(std::mem::size_of::<(&str, &str, &str)>())
        .context("field-semantics attribute allocation overflow")
}

struct NodeValue<'a, 'input>(roxmltree::Node<'a, 'input>);
struct Children<'a, 'input>(roxmltree::Node<'a, 'input>);

impl Serialize for Children<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        for child in self
            .0
            .children()
            .filter(|node| node.is_element() || node.is_text())
        {
            sequence.serialize_element(&NodeValue(child))?;
        }
        sequence.end()
    }
}

impl Serialize for NodeValue<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let node = self.0;
        if node.is_text() {
            let mut sequence = serializer.serialize_seq(Some(2))?;
            sequence.serialize_element("text")?;
            sequence.serialize_element(node.text().unwrap_or_default())?;
            return sequence.end();
        }
        let mut attributes = Vec::with_capacity(node.attributes().len());
        attributes.extend(node.attributes().map(|attribute| {
            (
                attribute.namespace().unwrap_or_default(),
                attribute.name(),
                attribute.value(),
            )
        }));
        // Equal triples emit identical bytes, so unstable ordering cannot alter
        // the previous stable-sort serialization and requires no sort buffer.
        attributes.sort_unstable();
        let mut sequence = serializer.serialize_seq(Some(4))?;
        sequence.serialize_element(node.tag_name().namespace().unwrap_or_default())?;
        sequence.serialize_element(node.tag_name().name())?;
        sequence.serialize_element(&attributes)?;
        sequence.serialize_element(&Children(node))?;
        sequence.end()
    }
}

struct Digest(blake3::Hasher);
impl Write for Digest {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn hash(node: roxmltree::Node<'_, '_>) -> Result<String> {
    let mut output = Digest(blake3::Hasher::new());
    serde_json::to_writer(&mut output, &NodeValue(node))?;
    Ok(output.0.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn previous(node: roxmltree::Node<'_, '_>) -> serde_json::Value {
        if node.is_text() {
            return serde_json::json!(["text", node.text().unwrap_or_default()]);
        }
        let mut attributes = node
            .attributes()
            .map(|attribute| {
                (
                    attribute.namespace().unwrap_or_default(),
                    attribute.name(),
                    attribute.value(),
                )
            })
            .collect::<Vec<_>>();
        attributes.sort();
        serde_json::json!([
            node.tag_name().namespace().unwrap_or_default(),
            node.tag_name().name(),
            attributes,
            node.children()
                .filter(|child| child.is_element() || child.is_text())
                .map(previous)
                .collect::<Vec<_>>()
        ])
    }

    #[test]
    fn streamed_json_matches_qualified_nested_unicode_and_mixed_text() -> Result<()> {
        for xml in [
            r#"<p:r xmlns:p="urn:one" xmlns:q="urn:two" z="last" q:a="é&amp;&quot;" a="first"><q:n>α</q:n> tail <![CDATA[<raw>]]><!-- ignored --></p:r>"#,
            r#"<r xmlns="urn:default"><s xml:lang="EN-US"><t a="1" b="2"/><t b="2" a="1"/></s></r>"#,
            r#"<r>whitespace \ " quote &amp; entity</r>"#,
        ] {
            let document = roxmltree::Document::parse(xml)?;
            for node in document
                .descendants()
                .filter(|node| node.is_element() || node.is_text())
            {
                let expected = serde_json::to_vec(&previous(node))?;
                assert_eq!(serde_json::to_vec(&NodeValue(node))?, expected);
                assert_eq!(hash(node)?, blake3::hash(&expected).to_hex().as_str());
            }
        }
        Ok(())
    }
}
