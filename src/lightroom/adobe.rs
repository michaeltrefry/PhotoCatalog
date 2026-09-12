//! Bounded interpretation of retained Adobe settings; never executes catalog text.
//! Parameter mapping is distinct from Adobe rendering equivalence.
use crate::edit::{Recipe, WhiteBalance};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[path = "adobe_data.rs"]
mod data;

pub const CRS: &str = "http://ns.adobe.com/camera-raw-settings/1.0/";
const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
/// Every agreed basic property is accounted for, including unrepresentable dependencies.
pub const BASIC_PROPERTIES: &[&str] = &[
    "ProcessVersion",
    "Version",
    "HasSettings",
    "HasCrop",
    "CropLeft",
    "CropTop",
    "CropRight",
    "CropBottom",
    "CropAngle",
    "CropWidth",
    "CropHeight",
    "CropUnits",
    "Exposure",
    "Exposure2012",
    "AutoExposure",
    "WhiteBalance",
    "Temperature",
    "Tint",
    "IncrementalTemperature",
    "IncrementalTint",
    "Contrast",
    "Contrast2012",
    "AutoContrast",
    "Highlights2012",
    "Shadows",
    "Shadows2012",
    "FillLight",
    "HighlightRecovery",
    "Brightness",
    "AutoBrightness",
    "AutoShadows",
    "Whites2012",
    "Blacks2012",
    "Saturation",
    "Vibrance",
    "Sharpness",
    "SharpenRadius",
    "SharpenDetail",
    "SharpenEdgeMasking",
    "LuminanceSmoothing",
    "ColorNoiseReduction",
    "LuminanceNoiseReductionDetail",
    "LuminanceNoiseReductionContrast",
    "ColorNoiseReductionDetail",
    "ColorNoiseReductionSmoothness",
];
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    CrsXml,
    CatalogData,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Raw,
    Raster,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Association {
    Current,
    Historical,
    Unresolved,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Key {
    Name(String),
    Index(u64),
    Xml {
        namespace: String,
        name: String,
        ordinal: u64,
    },
}
/// Locator is an external retained-record address, not a path opened by this module.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Input {
    pub source_id: String,
    pub revision: String,
    pub locator: String,
    pub payload_blake3: String,
    pub payload_bytes: u64,
    pub format: Format,
    pub source_kind: SourceKind,
    pub association: Association,
    /// Catalog table path explicitly selected by the source adapter; XML uses [].
    pub settings_path: Vec<Key>,
    pub as_shot_available: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub bytes: usize,
    pub properties: usize,
    pub tokens: usize,
    pub depth: usize,
    pub string_bytes: usize,
    pub output_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: 4 * 1024 * 1024,
            properties: 10_000,
            tokens: 50_000,
            depth: 32,
            string_bytes: 64 * 1024,
            output_bytes: 8 * 1024 * 1024,
        }
    }
}
impl Limits {
    fn validate(self) -> Result<()> {
        let d = Self::default();
        ensure!(
            self.bytes > 0
                && self.bytes <= d.bytes
                && self.properties > 0
                && self.properties <= d.properties
                && self.tokens > 0
                && self.tokens <= d.tokens
                && self.depth > 0
                && self.depth <= d.depth
                && self.string_bytes > 0
                && self.string_bytes <= d.string_bytes
                && self.output_bytes > 0
                && self.output_bytes <= d.output_bytes,
            "Adobe limits must be positive and within hard ceilings"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Value {
    Number(f64),
    Boolean(bool),
    Text(String),
    Null,
    Container,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    MappedParameter,
    RetainedOnly,
    Invalid,
    Conflict,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Property {
    pub path: Vec<Key>,
    pub namespace: Option<String>,
    pub name: String,
    /// Exact source token/element/attribute, not a reconstructed value.
    pub lexical: String,
    /// Offsets into decoded UTF-8 XML, or original UTF-8 catalog data. Never raw UTF-16 offsets.
    pub start: usize,
    pub end: usize,
    pub value: Value,
    pub disposition: Disposition,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    ResourceLimit,
    InvalidSyntax,
    Conflict,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Failure {}
impl Failure {
    fn syntax(message: &str) -> Self {
        Self {
            kind: FailureKind::InvalidSyntax,
            message: message.into(),
        }
    }
    fn limit(message: &str) -> Self {
        Self {
            kind: FailureKind::ResourceLimit,
            message: message.into(),
        }
    }
    fn conflict(message: &str) -> Self {
        Self {
            kind: FailureKind::Conflict,
            message: message.into(),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RecipeContribution {
    pub exposure_ev: Option<f32>,
    pub white_balance: Option<WhiteBalance>,
    /// Numerical DNG SDK white point, not proof of matching camera rendering.
    pub white_point_xy: Option<[f64; 2]>,
}
impl RecipeContribution {
    /// Apply only mapped properties to an explicitly supplied existing recipe.
    pub fn apply_to(&self, target: &Recipe) -> Result<crate::edit::ValidatedRecipe> {
        target.validate()?;
        let mut r = target.clone();
        let Recipe::V1(v) = &mut r;
        if let Some(ev) = self.exposure_ev {
            v.exposure_ev = ev;
        }
        if let Some(wb) = &self.white_balance {
            v.white_balance = wb.clone();
        }
        Ok(r.validate()?)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Extraction {
    pub input: Input,
    pub coordinate_space: String,
    pub properties: Vec<Property>,
    /// Missing means absent only after a complete parse; empty on parse failure.
    pub missing: Vec<String>,
    pub failure: Option<Failure>,
    pub contribution: RecipeContribution,
    pub adobe_rendering_equivalent: bool,
}
/// Allows adapters to report a fully retained oversized/unparseable external record.
/// It does not claim to have read, hashed or interpreted that external payload.
pub fn retained_failure(input: Input, failure: Failure) -> Extraction {
    Extraction {
        input,
        coordinate_space: "not_parsed".into(),
        properties: vec![],
        missing: vec![],
        failure: Some(failure),
        contribution: RecipeContribution::default(),
        adobe_rendering_equivalent: false,
    }
}
fn validate_input(input: &Input) -> Result<()> {
    ensure!(
        [&input.source_id, &input.revision, &input.locator]
            .iter()
            .all(|s| !s.is_empty() && s.len() <= 4096 && !s.contains('\0')),
        "missing or oversized retained Adobe source identity"
    );
    ensure!(
        input.payload_blake3.len() == 64
            && input
                .payload_blake3
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid payload BLAKE3"
    );
    ensure!(
        input.settings_path.len() <= 32
            && input
                .settings_path
                .iter()
                .all(|k| matches!(k, Key::Index(_))
                    || matches!(k,Key::Name(s) if !s.is_empty() && s.len()<=1024)),
        "invalid catalog settings path"
    );
    ensure!(
        input.format != Format::CrsXml || input.settings_path.is_empty(),
        "XML has no catalog settings path"
    );
    Ok(())
}
/// Select only a grammar-proven catalog settings container. This does not
/// qualify any values or reinterpret an explicit `Input::settings_path`.
/// Unknown assignment names and bounded parse failures remain retained data.
pub fn catalog_settings_path(
    bytes: &[u8],
    limits: Limits,
) -> Result<std::result::Result<Vec<Key>, Failure>> {
    limits.validate()?;
    if bytes.len() > limits.bytes {
        return Ok(Err(Failure::limit("catalog container byte limit")));
    }
    Ok(
        data::parse(bytes, limits).and_then(|parsed| match parsed.root {
            data::Root::Bare | data::Root::Return => Ok(vec![]),
            data::Root::Assignment(name) if name == "s" => Ok(vec![Key::Name(name)]),
            data::Root::Assignment(_) => Err(Failure::conflict(
                "unqualified catalog outer assignment; original data retained",
            )),
        }),
    )
}
/// Digest mismatch/invalid caller contract returns Err. Parse/limit failures return a
/// retained Extraction with no partial properties or recipe contributions.
pub fn extract(bytes: &[u8], input: Input, limits: Limits) -> Result<Extraction> {
    limits.validate()?;
    validate_input(&input)?;
    ensure!(
        bytes.len() as u64 == input.payload_bytes,
        "Adobe payload length mismatch"
    );
    if bytes.len() > limits.bytes {
        return Ok(retained_failure(
            input,
            Failure::limit(
                "payload exceeds extraction byte limit; external reference retained, digest not rechecked",
            ),
        ));
    }
    ensure!(
        blake3::hash(bytes).to_hex().as_str() == input.payload_blake3,
        "Adobe payload digest mismatch"
    );
    let parsed = match input.format {
        Format::CatalogData => data::parse(bytes, limits).map(|p| p.properties),
        Format::CrsXml => xml(bytes, limits),
    };
    let properties = match parsed {
        Ok(p) => p,
        Err(e) => return Ok(retained_failure(input, e)),
    };
    let mut result = Extraction {
        coordinate_space: if input.format == Format::CrsXml {
            "decoded_xml_utf8"
        } else {
            "original_utf8"
        }
        .into(),
        input,
        properties,
        missing: vec![],
        failure: None,
        contribution: RecipeContribution::default(),
        adobe_rendering_equivalent: false,
    };
    translate(&mut result);
    if super::bounded_json(&result, limits.output_bytes).is_err() {
        return Ok(retained_failure(
            result.input,
            Failure::limit(
                "extraction output exceeds limit; all original data remains externally retained",
            ),
        ));
    }
    Ok(result)
}
fn eligible(p: &Property, input: &Input) -> bool {
    match input.format {
        Format::CrsXml => p.namespace.as_deref() == Some(CRS) && p.path.len() == 1,
        Format::CatalogData => {
            p.namespace.is_none()
                && p.path.len() == input.settings_path.len() + 1
                && p.path.starts_with(&input.settings_path)
        }
    }
}
fn numeric_spec(name: &str) -> Option<(f64, f64, bool)> {
    Some(match name {
        // No coordinate convention is qualified yet; do not impose native ranges.
        "CropLeft" | "CropTop" | "CropRight" | "CropBottom" | "CropAngle" => {
            (f64::MIN, f64::MAX, false)
        }
        "CropWidth" | "CropHeight" => (0., f64::MAX, false),
        "CropUnits" => (0., 2., true),
        "Exposure" => (-4., 4., false),
        "Exposure2012" => (-5., 5., false),
        "Temperature" => (2000., 50000., true),
        "Tint" => (-150., 150., false),
        "Contrast" => (-50., 100., false),
        "Brightness" => (0., 150., false),
        "Contrast2012"
        | "Highlights2012"
        | "Shadows2012"
        | "Whites2012"
        | "Blacks2012"
        | "Saturation"
        | "Vibrance"
        | "IncrementalTemperature"
        | "IncrementalTint" => (-100., 100., false),
        "Sharpness" => (0., 150., false),
        "SharpenRadius" => (0.5, 3., false),
        "Shadows"
        | "FillLight"
        | "HighlightRecovery"
        | "SharpenDetail"
        | "SharpenEdgeMasking"
        | "LuminanceSmoothing"
        | "ColorNoiseReduction"
        | "LuminanceNoiseReductionDetail"
        | "LuminanceNoiseReductionContrast"
        | "ColorNoiseReductionDetail"
        | "ColorNoiseReductionSmoothness" => (0., 100., false),
        _ => return None,
    })
}
fn boolean_spec(name: &str) -> bool {
    name.starts_with("Enable")
        || matches!(
            name,
            "HasCrop"
                | "HasSettings"
                | "AutoExposure"
                | "AutoContrast"
                | "AutoBrightness"
                | "AutoShadows"
        )
}
fn recipe_number(value: f64) -> Option<f32> {
    let target = value as f32;
    (target.is_finite() && (value == 0.0 || target != 0.0)).then_some(target)
}
fn translate(r: &mut Extraction) {
    let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, p) in r.properties.iter_mut().enumerate() {
        if !eligible(p, &r.input) {
            continue;
        }
        by_name.entry(p.name.clone()).or_default().push(i);
        if let Some((mut min, mut max, mut integer)) = numeric_spec(&p.name) {
            if p.name == "Temperature" && r.input.source_kind != SourceKind::Raw {
                // A raster/unknown temperature does not have the proven raw Kelvin domain.
                min = f64::MIN;
                max = f64::MAX;
                integer = false;
            }
            let n = match &p.value {
                Value::Number(n) => Some(*n),
                Value::Text(s) if r.input.format == Format::CrsXml => data::number(s).ok(),
                _ => None,
            };
            if let Some(n) = n.filter(|n| {
                n.is_finite() && *n >= min && *n <= max && (!integer || n.fract() == 0.)
            }) {
                p.value = Value::Number(n);
            } else {
                p.disposition = Disposition::Invalid;
                p.reason =
                    "invalid numeric type, unit domain or finite range; exact source retained"
                        .into();
            }
        } else if boolean_spec(&p.name) {
            let v = match &p.value {
                Value::Boolean(v) => Some(*v),
                Value::Text(s) if r.input.format == Format::CrsXml => match s.as_str() {
                    "True" | "true" => Some(true),
                    "False" | "false" => Some(false),
                    _ => None,
                },
                _ => None,
            };
            if let Some(v) = v {
                p.value = Value::Boolean(v);
            } else {
                p.disposition = Disposition::Invalid;
                p.reason = "expected an explicit boolean".into();
            }
        } else if matches!(
            p.name.as_str(),
            "ProcessVersion" | "Version" | "WhiteBalance"
        ) && !matches!(p.value, Value::Text(_))
        {
            p.disposition = Disposition::Invalid;
            p.reason = "expected text".into();
        }
    }
    for indices in by_name.values().filter(|x| x.len() > 1) {
        for i in indices {
            r.properties[*i].disposition = Disposition::Conflict;
            r.properties[*i].reason = "duplicate source property; no value selected".into();
        }
    }
    r.missing = BASIC_PROPERTIES
        .iter()
        .filter(|n| !by_name.contains_key(**n))
        .map(|s| (*s).into())
        .collect();
    let value = |name: &str| -> Option<Value> {
        let ids = by_name.get(name)?;
        if ids.len() != 1 {
            return None;
        }
        let p = &r.properties[ids[0]];
        if p.disposition != Disposition::RetainedOnly {
            return None;
        }
        Some(p.value.clone())
    };
    // Serialized11.0 is explicitly present in Adobe's official CRS sample. Do not
    // guess other serialized versions from product numbers or UI process labels.
    let version = value("ProcessVersion");
    let switches_valid = !by_name
        .keys()
        .filter(|n| n.starts_with("Enable"))
        .any(|n| value(n) != Some(Value::Boolean(true)))
        && (!by_name.contains_key("HasSettings")
            || value("HasSettings") == Some(Value::Boolean(true)));
    let qualified = version == Some(Value::Text("11.0".into()))
        && r.input.association == Association::Current
        && r.input.source_kind != SourceKind::Unknown
        && switches_valid;
    let numeric = |name: &str| match value(name) {
        Some(Value::Number(n)) => Some(n),
        _ => None,
    };
    let mut mappings = vec![];
    if qualified {
        // Auto intent/old concurrent property cannot silently select a new EV.
        if matches!(value("AutoExposure"), None | Some(Value::Boolean(false)))
            && !by_name.contains_key("Exposure")
            && !by_name
                .get("AutoExposure")
                .is_some_and(|_| value("AutoExposure").is_none())
            && let Some(ev) = numeric("Exposure2012").and_then(recipe_number)
        {
            r.contribution.exposure_ev = Some(ev);
            mappings.push("Exposure2012");
        }
        if r.input.source_kind == SourceKind::Raw
            && !by_name.contains_key("IncrementalTemperature")
            && !by_name.contains_key("IncrementalTint")
        {
            match value("WhiteBalance") {
                Some(Value::Text(mode)) if mode == "Custom" => {
                    if let (Some(k), Some(tint)) = (
                        numeric("Temperature"),
                        numeric("Tint").and_then(recipe_number),
                    ) {
                        let wb = WhiteBalance::TemperatureTint {
                            kelvin: k as u32,
                            tint,
                        };
                        if let Ok(Some(xy)) = crate::edit::color::white_point(&wb) {
                            r.contribution.white_point_xy = Some(xy);
                            r.contribution.white_balance = Some(wb);
                            mappings.extend(["WhiteBalance", "Temperature", "Tint"]);
                        }
                    }
                }
                Some(Value::Text(mode))
                    if mode == "As Shot"
                        && r.input.as_shot_available
                        && !by_name.contains_key("Temperature")
                        && !by_name.contains_key("Tint") =>
                {
                    r.contribution.white_balance = Some(WhiteBalance::AsShot);
                    mappings.push("WhiteBalance");
                }
                _ => {}
            }
        }
    }
    for p in &mut r.properties {
        if !eligible(p, &r.input) || p.disposition != Disposition::RetainedOnly {
            continue;
        }
        if mappings.contains(&p.name.as_str()) {
            p.disposition = Disposition::MappedParameter;
            p.reason="validated numeric parameter mapping; native rendering is not Adobe rendering equivalence".into();
        } else if !qualified {
            p.reason="source version, kind, current-state association or enable switches not qualified; retained without applying".into();
        } else if p.name.starts_with("Crop") || p.name == "HasCrop" {
            p.reason="crop orientation, center, angle and canvas convention require coordinate reference proof".into();
        } else if BASIC_PROPERTIES.contains(&p.name.as_str()) {
            p.reason="retained: control dependencies, units or processing semantics are not qualified for application".into();
        }
    }
}
fn xml(bytes: &[u8], limits: Limits) -> std::result::Result<Vec<Property>, Failure> {
    let text = crate::xmp::xml_text(bytes).map_err(|_| Failure::syntax("invalid XML encoding"))?;
    if text.len() > limits.bytes {
        return Err(Failure::limit("decoded XML byte limit"));
    }
    let doc = roxmltree::Document::parse_with_options(
        &text,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: limits.tokens as u32,
            ..Default::default()
        },
    )
    .map_err(|_| Failure::syntax("XML parser limit or syntax"))?;
    let mut names = std::collections::BTreeSet::new();
    for n in doc.descendants().filter(|n| n.is_element()) {
        if n.ancestors().count() > limits.depth + 3 {
            return Err(Failure::limit("XML nesting limit"));
        }
        if n.has_tag_name((RDF, "Description"))
            && n.parent().is_some_and(|p| p.has_tag_name((RDF, "RDF")))
        {
            for a in n.attributes().filter(|a| a.namespace() != Some(RDF)) {
                if !names.insert((a.namespace().unwrap_or(""), a.name())) {
                    return Err(Failure::conflict("duplicate XML property"));
                }
            }
            for child in n.children().filter(|x| x.is_element()) {
                if !names.insert((
                    child.tag_name().namespace().unwrap_or(""),
                    child.tag_name().name(),
                )) {
                    return Err(Failure::conflict("duplicate XML property"));
                }
            }
        }
    }
    // Require existing native XMP and independent RDF preservation validation,
    // after our stricter input/node/depth admission and before interpretation.
    crate::xmp::parse(bytes)
        .map_err(|_| Failure::syntax("XMP model invalid or unsupported; full packet retained"))?;
    let mut out = vec![];
    for description in doc.descendants().filter(|n| {
        n.has_tag_name((RDF, "Description"))
            && n.parent().is_some_and(|p| p.has_tag_name((RDF, "RDF")))
    }) {
        // An external RDF subject is not silently associated with this photograph.
        if description
            .attribute((RDF, "about"))
            .is_some_and(|s| !s.is_empty())
        {
            return Err(Failure::syntax(
                "nonempty RDF subject requires explicit source association",
            ));
        }
        for a in description
            .attributes()
            .filter(|a| a.namespace() != Some(RDF))
        {
            let ns = a.namespace().unwrap_or("");
            let range = a.range();
            push(
                &mut out,
                Property {
                    path: vec![Key::Xml {
                        namespace: ns.into(),
                        name: a.name().into(),
                        ordinal: 0,
                    }],
                    namespace: Some(ns.into()),
                    name: a.name().into(),
                    lexical: text[range.clone()].into(),
                    start: range.start,
                    end: range.end,
                    value: Value::Text(a.value().into()),
                    disposition: Disposition::RetainedOnly,
                    reason: "unknown or unqualified property retained".into(),
                },
                limits,
            )?;
        }
        for n in description.children().filter(|n| n.is_element()) {
            xml_node(n, &text, vec![], &mut out, limits)?;
        }
    }
    Ok(out)
}
fn xml_node(
    n: roxmltree::Node<'_, '_>,
    text: &str,
    mut path: Vec<Key>,
    out: &mut Vec<Property>,
    limits: Limits,
) -> std::result::Result<(), Failure> {
    let name = n.tag_name().name();
    let ns = n.tag_name().namespace().unwrap_or("");
    let ordinal = n
        .prev_siblings()
        .filter(|p| p.is_element() && p.tag_name() == n.tag_name())
        .count() as u64;
    path.push(Key::Xml {
        namespace: ns.into(),
        name: name.into(),
        ordinal,
    });
    if path.len() > limits.depth {
        return Err(Failure::limit("XML property depth"));
    }
    let range = n.range();
    let simple = !n.children().any(|x| x.is_element()) && n.attributes().len() == 0;
    push(
        out,
        Property {
            path: path.clone(),
            namespace: Some(ns.into()),
            name: name.into(),
            lexical: text[range.clone()].into(),
            start: range.start,
            end: range.end,
            value: if simple {
                Value::Text(
                    n.children()
                        .filter(|x| x.is_text())
                        .filter_map(|x| x.text())
                        .collect(),
                )
            } else {
                Value::Container
            },
            disposition: Disposition::RetainedOnly,
            reason: "unknown or unqualified property retained".into(),
        },
        limits,
    )?;
    for a in n.attributes() {
        let range = a.range();
        let mut ap = path.clone();
        ap.push(Key::Xml {
            namespace: a.namespace().unwrap_or("").into(),
            name: format!("@{}", a.name()),
            ordinal: 0,
        });
        push(
            out,
            Property {
                path: ap,
                namespace: a.namespace().map(str::to_owned),
                name: a.name().into(),
                lexical: text[range.clone()].into(),
                start: range.start,
                end: range.end,
                value: Value::Text(a.value().into()),
                disposition: Disposition::RetainedOnly,
                reason: "qualifier or nested data retained".into(),
            },
            limits,
        )?;
    }
    for child in n.children().filter(|x| x.is_element()) {
        xml_node(child, text, path.clone(), out, limits)?;
    }
    Ok(())
}
fn push(out: &mut Vec<Property>, p: Property, limits: Limits) -> std::result::Result<(), Failure> {
    if out.len() >= limits.properties
        || p.lexical.len() > limits.bytes
        || matches!(&p.value,Value::Text(s) if s.len()>limits.string_bytes)
    {
        return Err(Failure::limit("property or string limit"));
    }
    out.push(p);
    Ok(())
}
#[cfg(test)]
#[path = "adobe_tests.rs"]
mod tests;
