//! Immutable metadata source evidence, indexed projections and explicit reconciliation.
use crate::{
    Catalog, location_bytes,
    xmp::{self, Edit, Projection, Value},
    xmp_packets::{self, Inspection, Limits, Status},
};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize, ser::SerializeSeq};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use crate::lightroom_migration_worker::memory::{
    ResourceLimit,
    layout::{add, mul, tree, vector},
    requested::{Requested, Scope},
};

struct JsonCount(usize);
impl Write for JsonCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("JSON length overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn json_bytes(value: &impl Serialize) -> Result<usize> {
    let mut count = JsonCount(0);
    serde_json::to_writer(&mut count, value)?;
    Ok(count.0)
}

#[derive(Serialize)]
struct PacketDescriptor<'a> {
    attributes: &'a BTreeMap<String, String>,
    container: &'a xmp_packets::Container,
    group: &'a str,
    ranges: &'a [xmp_packets::ByteRange],
}
#[derive(Serialize)]
struct ParseDescriptor<'a> {
    group: &'a str,
    packet_indices: &'a [usize],
    transformation: &'a xmp_packets::Transformation,
}
struct CombinedIndices<'a> {
    first: &'a [usize],
    second: &'a [usize],
}
impl Serialize for CombinedIndices<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.first.len() + self.second.len()))?;
        for index in self.first.iter().chain(self.second) {
            sequence.serialize_element(index)?;
        }
        sequence.end()
    }
}
#[derive(Serialize)]
struct MergedDescriptor<'a> {
    derived_from_inputs: [usize; 2],
    guid: &'a str,
    packet_indices: CombinedIndices<'a>,
    transformation: &'static str,
}
#[derive(Serialize)]
struct SourceLocation<'a> {
    display: &'a str,
    kind: &'a str,
    locator: &'a [u8],
}
#[derive(Serialize)]
struct PreparedProvenance<'a> {
    file_revision: &'a xmp_packets::SourceRevision,
    source: &'a serde_json::Value,
    source_location: SourceLocation<'a>,
}

#[derive(Deserialize)]
struct BorrowedRevision {
    file_revision: xmp_packets::SourceRevision,
}
#[derive(Serialize)]
struct CanonicalSourceRevision<'a> {
    blake3: &'a str,
    length: u64,
    modified_unix_ns: Option<u128>,
}
struct ModelIdentities<'a>(&'a [PreparedModel]);
impl Serialize for ModelIdentities<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for model in self.0 {
            sequence.serialize_element(&(
                &model.hash,
                &model.descriptor,
                &model.projection,
                &model.error,
            ))?;
        }
        sequence.end()
    }
}
#[derive(Serialize)]
struct RevisionIdentity<'a> {
    issues: &'a str,
    models: ModelIdentities<'a>,
    packets: &'a [(String, String)],
    provenance: &'a str,
    source_revision: CanonicalSourceRevision<'a>,
    status: &'a str,
    version: u8,
}
struct JsonDigest(blake3::Hasher);
impl Write for JsonDigest {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) const SCHEMA: &str = "
ALTER TABLE assets ADD COLUMN render_generation INTEGER NOT NULL DEFAULT 0;
CREATE TABLE metadata_assets(asset_id TEXT PRIMARY KEY REFERENCES assets(id), revision INTEGER NOT NULL DEFAULT 0);
CREATE TABLE metadata_blobs(hash TEXT PRIMARY KEY, raw_length INTEGER NOT NULL CHECK(raw_length>=0 AND raw_length<=16777216), compressed BLOB NOT NULL);
CREATE TABLE metadata_sources(id INTEGER PRIMARY KEY, asset_id TEXT NOT NULL REFERENCES assets(id), kind TEXT NOT NULL, locator BLOB NOT NULL, display TEXT NOT NULL, association TEXT NOT NULL, availability TEXT NOT NULL, current_observation INTEGER, UNIQUE(asset_id,kind,locator));
CREATE INDEX metadata_sources_asset ON metadata_sources(asset_id);
CREATE TABLE metadata_observations(id INTEGER PRIMARY KEY, source_id INTEGER NOT NULL REFERENCES metadata_sources(id), revision TEXT NOT NULL, status TEXT NOT NULL, issues TEXT NOT NULL, provenance TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%fZ','now')), UNIQUE(source_id,revision));
CREATE TABLE metadata_packets(observation_id INTEGER NOT NULL REFERENCES metadata_observations(id), ordinal INTEGER NOT NULL, blob_hash TEXT NOT NULL REFERENCES metadata_blobs(hash), descriptor TEXT NOT NULL, PRIMARY KEY(observation_id,ordinal));
CREATE TABLE metadata_models(id INTEGER PRIMARY KEY, observation_id INTEGER NOT NULL REFERENCES metadata_observations(id), ordinal INTEGER NOT NULL, blob_hash TEXT NOT NULL REFERENCES metadata_blobs(hash), descriptor TEXT NOT NULL, projection TEXT NOT NULL, error TEXT, UNIQUE(observation_id,ordinal));
CREATE TABLE metadata_values(model_id INTEGER NOT NULL REFERENCES metadata_models(id), field TEXT NOT NULL, value TEXT NOT NULL, semantic_hash TEXT NOT NULL, PRIMARY KEY(model_id,field));
CREATE INDEX metadata_values_field ON metadata_values(field,value,model_id);
CREATE TABLE metadata_choices(asset_id TEXT NOT NULL REFERENCES assets(id), field TEXT NOT NULL, model_id INTEGER NOT NULL REFERENCES metadata_models(id), PRIMARY KEY(asset_id,field));
CREATE TABLE metadata_history(id INTEGER PRIMARY KEY, asset_id TEXT NOT NULL REFERENCES assets(id), revision INTEGER NOT NULL, action TEXT NOT NULL, detail TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%fZ','now')));
CREATE INDEX metadata_history_asset ON metadata_history(asset_id,id);
CREATE TABLE metadata_effective(asset_id TEXT NOT NULL REFERENCES assets(id), field TEXT NOT NULL, value TEXT, conflicted INTEGER NOT NULL CHECK(conflicted IN(0,1)), model_id INTEGER REFERENCES metadata_models(id), PRIMARY KEY(asset_id,field));
CREATE INDEX metadata_effective_lookup ON metadata_effective(field,value,asset_id);
CREATE TABLE metadata_export_plans(operation TEXT PRIMARY KEY,asset_id TEXT NOT NULL REFERENCES assets(id),revision INTEGER NOT NULL,base_model INTEGER NOT NULL REFERENCES metadata_models(id),plan TEXT NOT NULL,payload_hash TEXT NOT NULL REFERENCES metadata_blobs(hash),receipt TEXT);
";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub kind: String,
    /// Lossless platform-native location, or an importer-defined stable opaque key.
    pub locator: Vec<u8>,
    pub display: String,
    /// Ambiguous sidecar associations are retained but never silently selected.
    pub ambiguous: bool,
    pub provenance: serde_json::Value,
}
#[derive(Debug, Serialize)]
pub struct Candidate {
    pub model_id: i64,
    pub source_id: i64,
    pub source_kind: String,
    pub source_display: String,
    pub observation_id: i64,
    pub ambiguous: bool,
    pub value: Value,
    pub semantic_hash: String,
}
#[derive(Debug, Serialize)]
pub struct Field {
    pub name: String,
    pub value: Option<Value>,
    pub conflicted: bool,
    pub selected_model: Option<i64>,
    pub candidates: Vec<Candidate>,
}
#[derive(Debug, Serialize)]
pub struct SourceState {
    pub id: i64,
    pub kind: String,
    pub display: String,
    pub association: String,
    pub availability: String,
    pub observation_id: Option<i64>,
    pub status: Option<String>,
    pub issues: Option<serde_json::Value>,
}
#[derive(Debug, Serialize)]
pub struct MetadataView {
    pub asset_id: String,
    pub revision: i64,
    pub sources: Vec<SourceState>,
    pub fields: Vec<Field>,
}
#[derive(Debug, Serialize)]
pub struct Observation {
    pub id: i64,
    pub source_id: i64,
    pub revision: String,
    pub status: String,
    pub issues: serde_json::Value,
    pub provenance: serde_json::Value,
    pub current: bool,
    pub models: Vec<ModelSummary>,
}
#[derive(Debug, Serialize)]
pub struct ModelSummary {
    pub id: i64,
    pub ordinal: i64,
    pub blob_hash: String,
    pub descriptor: serde_json::Value,
    pub projection: Projection,
    pub error: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct PacketEvidence {
    pub ordinal: i64,
    pub descriptor: serde_json::Value,
    pub bytes: Vec<u8>,
    pub blake3: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenderIdentity {
    pub asset_id: String,
    pub generation: i64,
    pub fingerprint: Option<String>,
    pub state: String,
    pub metadata_revision: i64,
}
#[derive(Debug, Serialize)]
pub struct Change {
    pub revision: i64,
    pub observation_id: i64,
    pub model_ids: Vec<i64>,
    pub changed: bool,
}

pub(crate) const FILE_INSTANCE_SCHEMA: &str = "
CREATE TABLE metadata_file_instances(id INTEGER PRIMARY KEY, asset_id TEXT NOT NULL REFERENCES assets(id), source_id INTEGER NOT NULL REFERENCES metadata_sources(id), evidence_hash TEXT NOT NULL, provenance TEXT NOT NULL, observed_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%fZ','now')), UNIQUE(source_id,evidence_hash));
CREATE INDEX metadata_file_instances_asset ON metadata_file_instances(asset_id,id);
";

/// File-instance observations preserve relocation/copy timing without replacing an
/// unchanged immutable XMP observation or invalidating selected model identities.
#[derive(Debug, Serialize)]
pub struct FileInstance {
    pub id: i64,
    pub source_id: i64,
    pub provenance: serde_json::Value,
    pub observed_at: String,
}

/// In-memory prepared edit; fields are private so only validated preparation
/// can produce this authority. Catalog workers inherit the exact session Arc.
pub(crate) struct PreparedEdit {
    catalog_file: std::sync::Arc<crate::catalog_session::CatalogSessionAuthority>,
    image_identity: crate::catalog_images::ImageMetadataIdentity,
    expected_revision: i64,
    base_model: Option<i64>,
    edits: Vec<Edit>,
    organization_fields: Vec<String>,
    source: Source,
    prepared: Prepared,
    original: Projection,
    original_semantics: BTreeMap<String, String>,
    updated: Projection,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PreparedEditSummary {
    pub identity: crate::catalog_images::ImageMetadataIdentity,
    pub base_model: Option<i64>,
    pub edits: Vec<Edit>,
    pub before: Projection,
    pub after: Projection,
    pub packet_bytes: u64,
    pub packet_blake3: String,
    pub issues: Vec<String>,
}

impl PreparedEdit {
    pub(crate) fn summary(&self) -> Result<PreparedEditSummary> {
        let model = self
            .prepared
            .models
            .first()
            .context("prepared edit has no model")?;
        let (length, _) = self
            .prepared
            .blobs
            .get(&model.hash)
            .context("prepared edit packet missing")?;
        Ok(PreparedEditSummary {
            identity: self.image_identity.clone(),
            base_model: self.base_model,
            edits: self.edits.clone(),
            before: self.original.clone(),
            after: self.updated.clone(),
            packet_bytes: u64::try_from(*length)?,
            packet_blake3: model.hash.clone(),
            issues: self.updated.issues.clone(),
        })
    }
    pub(crate) fn packet(&self) -> Result<Vec<u8>> {
        let model = self
            .prepared
            .models
            .first()
            .context("prepared edit has no model")?;
        let (length, compressed) = self
            .prepared
            .blobs
            .get(&model.hash)
            .context("prepared edit packet missing")?;
        let decoder = ZlibDecoder::new(compressed.as_slice());
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(*length)?;
        decoder
            .take(u64::try_from(*length)?.saturating_add(1))
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() == *length && blake3::hash(&bytes).to_hex().as_str() == model.hash,
            "prepared edit packet verification"
        );
        Ok(bytes)
    }
}

struct PreparedModel {
    hash: String,
    semantics: BTreeMap<String, String>,
    descriptor: String,
    projection: Projection,
    error: Option<String>,
}
pub(crate) struct Prepared {
    revision: String,
    status: String,
    issues: String,
    provenance: String,
    blobs: BTreeMap<String, (usize, Vec<u8>)>,
    packets: Vec<(String, String)>,
    models: Vec<PreparedModel>,
}
pub(crate) struct AdmittedPrepared<'a> {
    pub(crate) value: Prepared,
    // Struct fields drop in declaration order, so this outlives value.
    _scope: Scope<'a>,
}
impl Prepared {
    pub(crate) fn new(inspection: &Inspection, source: &Source) -> Result<Self> {
        let admit = |_| Ok(());
        let requested = Requested::new(&admit);
        Ok(Self::new_admitted(inspection, source, &requested)?.value)
    }

    pub(crate) fn new_admitted<'a>(
        inspection: &Inspection,
        source: &Source,
        requested: &'a Requested<'a>,
    ) -> Result<AdmittedPrepared<'a>> {
        ensure!(
            inspection.packets.len() <= 1024 && inspection.parse_inputs.len() <= 1024,
            "metadata observation exceeds packet count limit"
        );
        ensure!(
            inspection
                .packets
                .iter()
                .map(|p| p.bytes.len() as u64)
                .sum::<u64>()
                <= 64 * 1024 * 1024
                && inspection
                    .parse_inputs
                    .iter()
                    .map(|p| p.bytes.len() as u64)
                    .sum::<u64>()
                    <= 64 * 1024 * 1024,
            "metadata observation exceeds retained/parse byte limit"
        );
        let prepared_scope = requested.scope(prepared_storage(inspection, source)?)?;
        let mut value = Self {
            revision: String::new(),
            status: format!("{:?}", inspection.status),
            issues: serde_json::to_string(&inspection.issues)?,
            provenance: serde_json::to_string(&PreparedProvenance {
                file_revision: &inspection.revision,
                source: &source.provenance,
                source_location: SourceLocation {
                    display: &source.display,
                    kind: &source.kind,
                    locator: &source.locator,
                },
            })?,
            blobs: BTreeMap::new(),
            packets: Vec::new(),
            models: Vec::new(),
        };
        for packet in &inspection.packets {
            let hash = value.blob(&packet.bytes)?;
            ensure!(hash == packet.blake3, "packet digest mismatch");
            value.packets.push((
                hash,
                serde_json::to_string(&PacketDescriptor {
                    attributes: &packet.attributes,
                    container: &packet.container,
                    group: &packet.group,
                    ranges: &packet.ranges,
                })?,
            ));
        }
        for input in &inspection.parse_inputs {
            ensure!(
                input
                    .packet_indices
                    .iter()
                    .all(|i| *i < inspection.packets.len()),
                "parse input references missing packet"
            );
            let hash = value.blob(&input.bytes)?;
            ensure!(hash == input.blake3, "parse input digest mismatch");
            let (projection, error) = match xmp::project_admitted(&input.bytes, requested) {
                Ok(p) => (p, None),
                Err(error) if error.downcast_ref::<ResourceLimit>().is_some() => return Err(error),
                Err(e) => (Projection::default(), Some(format!("{e:#}"))),
            };
            value.models.push(PreparedModel {
                semantics: if error.is_none() {
                    xmp::field_semantics_admitted(&input.bytes, requested)?
                } else {
                    BTreeMap::new()
                },
                hash,
                descriptor: serde_json::to_string(&ParseDescriptor {
                    group: &input.group,
                    packet_indices: &input.packet_indices,
                    transformation: &input.transformation,
                })?,
                projection,
                error,
            });
        }
        // Extended JPEG data is linked by its declared GUID, never by adjacency.
        let mut joined = BTreeSet::new();
        for (index, input) in inspection.parse_inputs.iter().enumerate() {
            if !input
                .packet_indices
                .iter()
                .any(|i| inspection.packets[*i].container == xmp_packets::Container::JpegMain)
            {
                continue;
            }
            let meta = match xmp::parse_admitted(&input.bytes, requested) {
                Ok(meta) => meta,
                Err(error) if error.downcast_ref::<ResourceLimit>().is_some() => return Err(error),
                Err(_) => continue,
            };
            let Some(guid) = meta.property("http://ns.adobe.com/xmp/note/", "HasExtendedXMP")
            else {
                continue;
            };
            let prefix = input
                .group
                .split(":main:")
                .next()
                .context("JPEG main group missing")?;
            let target = format!("{prefix}:extended:{}", guid.value.to_ascii_uppercase());
            let mut extensions = inspection.parse_inputs.iter().enumerate().filter(|(_, p)| {
                p.group == target
                    && p.transformation == xmp_packets::Transformation::JpegExtendedReassembled
            });
            let extension = extensions.next();
            value.models[index].projection = Projection::default();
            if extension.is_none() || extensions.next().is_some() {
                value.models[index].error=Some("JPEG extended XMP association is missing or ambiguous; original main packet retained".into());
                continue;
            }
            let (extension_index, extension) = extension.unwrap();
            joined.insert(extension_index);
            match xmp::merge_jpeg_admitted(&input.bytes, &extension.bytes, requested) {
                Ok(merged) => {
                    let bytes = &merged.0;
                    let hash = value.blob(bytes)?;
                    value.models.push(PreparedModel {
                        semantics: xmp::field_semantics_admitted(bytes, requested)?,
                        hash,
                        descriptor: serde_json::to_string(&MergedDescriptor {
                            derived_from_inputs: [index, extension_index],
                            guid: &guid.value,
                            packet_indices: CombinedIndices {
                                first: &input.packet_indices,
                                second: &extension.packet_indices,
                            },
                            transformation: "JpegMainAndExtendedMerged",
                        })?,
                        projection: xmp::project_admitted(bytes, requested)?,
                        error: None,
                    });
                    value.models[index].error = Some(
                        "JPEG main fragment; use the associated merged model for editing/export"
                            .into(),
                    );
                }
                Err(error) if error.downcast_ref::<ResourceLimit>().is_some() => return Err(error),
                Err(error) => {
                    value.models[index].error = Some(format!(
                        "JPEG main/extended reconciliation failed: {error:#}"
                    ))
                }
            }
        }
        for (index, input) in inspection.parse_inputs.iter().enumerate() {
            if input.transformation == xmp_packets::Transformation::JpegExtendedReassembled {
                value.models[index].projection = Projection::default();
                value.models[index].error=Some(if joined.contains(&index) {"JPEG extended fragment; see associated main model"} else {"unassociated JPEG extended fragment retained; explicit association review required"}.into());
            }
        }
        // Distinguish repeated observations with changed extraction/parser status as well as bytes.
        value.revision = value.revision_with_provenance(&value.provenance)?;
        Ok(AdmittedPrepared {
            value,
            _scope: prepared_scope,
        })
    }
    fn revision_with_provenance(&self, provenance: &str) -> Result<String> {
        let parsed: BorrowedRevision = serde_json::from_str(provenance)?;
        let mut digest = JsonDigest(blake3::Hasher::new());
        serde_json::to_writer(
            &mut digest,
            &RevisionIdentity {
                issues: &self.issues,
                models: ModelIdentities(&self.models),
                packets: &self.packets,
                provenance,
                source_revision: CanonicalSourceRevision {
                    blake3: &parsed.file_revision.blake3,
                    length: parsed.file_revision.length,
                    modified_unix_ns: parsed.file_revision.modified_unix_ns,
                },
                status: &self.status,
                version: 1,
            },
        )?;
        Ok(digest.0.finalize().to_hex().to_string())
    }
    /// Compare the current observation with its original file-instance provenance.
    /// File bytes, source provenance, packet layout and parser results must all match.
    /// Location and timestamp describe a file instance, including same-path copies.
    /// Their new provenance is retained separately from the immutable model.
    fn matches_relocated_observation(&self, db: &Connection, id: i64) -> Result<bool> {
        let (revision, provenance): (String, String) = db.query_row(
            "SELECT revision,provenance FROM metadata_observations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut previous: serde_json::Value = serde_json::from_str(&provenance)?;
        let mut current: serde_json::Value = serde_json::from_str(&self.provenance)?;
        previous
            .as_object_mut()
            .context("invalid previous provenance")?
            .remove("source_location");
        current
            .as_object_mut()
            .context("invalid current provenance")?
            .remove("source_location");
        for value in [&mut previous, &mut current] {
            value
                .get_mut("file_revision")
                .and_then(serde_json::Value::as_object_mut)
                .context("invalid file revision")?
                .remove("modified_unix_ns");
        }
        Ok(previous == current && self.revision_with_provenance(&provenance)? == revision)
    }
    fn blob(&mut self, bytes: &[u8]) -> Result<String> {
        ensure!(
            bytes.len() <= xmp::MAX_PACKET_BYTES,
            "metadata blob exceeds retention limit"
        );
        let hash = blake3::hash(bytes).to_hex().to_string();
        if !self.blobs.contains_key(&hash) {
            let mut encoder = ZlibEncoder::new(
                Vec::with_capacity(zlib_bound(bytes.len())?),
                Compression::fast(),
            );
            encoder.write_all(bytes)?;
            self.blobs
                .insert(hash.clone(), (bytes.len(), encoder.finish()?));
        }
        Ok(hash)
    }
}

fn zlib_bound(bytes: usize) -> Result<usize> {
    // zlib's public compressBound expression, including wrapper bytes.
    add(
        bytes,
        add(add(bytes >> 12, bytes >> 14)?, add(bytes >> 25, 13)?)?,
    )
}

fn merged_descriptor_storage(inspection: &Inspection) -> Result<(usize, usize)> {
    if !inspection
        .parse_inputs
        .iter()
        .any(|input| input.transformation == xmp_packets::Transformation::JpegExtendedReassembled)
    {
        return Ok((0, 0));
    }
    let mut mains = 0usize;
    let mut bytes = 0usize;
    for (index, input) in inspection.parse_inputs.iter().enumerate() {
        if !input.packet_indices.iter().any(|packet| {
            inspection
                .packets
                .get(*packet)
                .is_some_and(|packet| packet.container == xmp_packets::Container::JpegMain)
        }) {
            continue;
        }
        mains = add(mains, 1)?;
        let fixed = inspection
            .parse_inputs
            .iter()
            .enumerate()
            .filter(|(_, extension)| {
                extension.transformation == xmp_packets::Transformation::JpegExtendedReassembled
            })
            .try_fold(0usize, |maximum, (extension_index, extension)| {
                Ok::<_, anyhow::Error>(maximum.max(json_bytes(&MergedDescriptor {
                    derived_from_inputs: [index, extension_index],
                    guid: "",
                    packet_indices: CombinedIndices {
                        first: &input.packet_indices,
                        second: &extension.packet_indices,
                    },
                    transformation: "JpegMainAndExtendedMerged",
                })?))
            })?;
        // serde_json can spell one source byte as a six-byte \u00XX escape.
        // The GUID is borrowed from this main's decoded XMP and cannot exceed
        // the encoding-derived maximum valid UTF-8 length.
        bytes = add(
            bytes,
            add(fixed, mul(6, xmp::decoded_text_bound(&input.bytes)?)?)?,
        )?;
    }
    Ok((mains, bytes))
}

/// Persistent Prepared owners are admitted before the first String, Vec, map
/// node or compressed blob is constructed. Counts come from the selected
/// Inspection. One generated merged model is possible for every actual JPEG
/// main; the same extension may lawfully feed all of them, so merged storage is
/// multiplied by main count rather than extension count.
fn prepared_storage(inspection: &Inspection, source: &Source) -> Result<usize> {
    let (mains, merged_descriptors) = merged_descriptor_storage(inspection)?;
    let models = add(inspection.parse_inputs.len(), mains)?;
    let blobs = add(
        add(inspection.packets.len(), inspection.parse_inputs.len())?,
        mains,
    )?;
    let mut compressed = 0usize;
    let mut source_error_bytes = 0usize;
    for bytes in inspection
        .packets
        .iter()
        .map(|packet| packet.bytes.len())
        .chain(
            inspection
                .parse_inputs
                .iter()
                .map(|input| input.bytes.len()),
        )
    {
        compressed = add(compressed, zlib_bound(bytes)?)?;
    }
    for input in &inspection.parse_inputs {
        source_error_bytes = add(source_error_bytes, xmp::decoded_text_bound(&input.bytes)?)?;
    }
    compressed = add(compressed, mul(mains, zlib_bound(xmp::MAX_PACKET_BYTES)?)?)?;
    // Prepared retains every earlier projection and semantics map while the
    // next model is built. The fixed model expression includes the existing
    // 17-field/10,000-item grammar and one compact packet's value payload.
    let interpreted = mul(models, xmp::prepared_model_storage()?)?;

    let packet_strings = inspection
        .packets
        .iter()
        .try_fold(0usize, |bytes, packet| {
            add(
                bytes,
                json_bytes(&PacketDescriptor {
                    attributes: &packet.attributes,
                    container: &packet.container,
                    group: &packet.group,
                    ranges: &packet.ranges,
                })?,
            )
        })?;
    let input_strings = inspection
        .parse_inputs
        .iter()
        .try_fold(0usize, |bytes, input| {
            add(
                bytes,
                json_bytes(&ParseDescriptor {
                    group: &input.group,
                    packet_indices: &input.packet_indices,
                    transformation: &input.transformation,
                })?,
            )
        })?;
    let issue_strings = json_bytes(&inspection.issues)?;
    let provenance_strings = json_bytes(&PreparedProvenance {
        file_revision: &inspection.revision,
        source: &source.provenance,
        source_location: SourceLocation {
            display: &source.display,
            kind: &source.kind,
            locator: &source.locator,
        },
    })?;
    let containers = add(
        vector::<(String, String)>(inspection.packets.len())?,
        add(
            vector::<PreparedModel>(models)?,
            tree::<String, (usize, Vec<u8>)>(blobs)?,
        )?,
    )?;
    let fixed_strings = mul(
        64,
        add(add(add(blobs, inspection.packets.len())?, models)?, 2)?,
    )?;
    // At most one retained error belongs to each input/generated model. Source
    // names and XML excerpts cannot exceed two copies of the actual decoded
    // input family; constant diagnostics are the longest local catch-all text.
    let diagnostic = [
        "JPEG extended XMP association is missing or ambiguous; original main packet retained",
        "JPEG main fragment; use the associated merged model for editing/export",
        "unassociated JPEG extended fragment retained; explicit association review required",
    ]
    .iter()
    .map(|text| text.len())
    .max()
    .unwrap_or(0);
    let errors = add(mul(2, source_error_bytes)?, mul(models, diagnostic)?)?;
    add(
        add(compressed, interpreted)?,
        add(
            containers,
            add(
                fixed_strings,
                add(
                    add(add(packet_strings, input_strings)?, merged_descriptors)?,
                    add(add(issue_strings, provenance_strings)?, errors)?,
                )?,
            )?,
        )?,
    )
}

#[cfg(test)]
mod prepared_admission_tests {
    use super::*;
    use std::cell::RefCell;

    const XMP: &[u8] = br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="2"/></rdf:RDF>"#;

    fn empty() -> (Inspection, Source) {
        (
            Inspection {
                revision: xmp_packets::SourceRevision {
                    length: 0,
                    blake3: "0".repeat(64),
                    modified_unix_ns: None,
                },
                status: Status::Absent,
                packets: Vec::new(),
                parse_inputs: Vec::new(),
                issues: Vec::new(),
            },
            Source {
                kind: "selected".into(),
                locator: b"fixture".to_vec(),
                display: "fixture".into(),
                ambiguous: false,
                provenance: serde_json::json!({"fixture":true}),
            },
        )
    }

    #[test]
    fn prepared_denial_is_typed_and_precedes_first_prepared_owner() -> Result<()> {
        let (inspection, source) = empty();
        let calls = RefCell::new(Vec::new());
        let admit = |required| {
            calls.borrow_mut().push(required);
            Err(ResourceLimit {
                required,
                available: required.saturating_sub(1),
            }
            .into())
        };
        let requested = Requested::new(&admit);
        let error = match Prepared::new_admitted(&inspection, &source, &requested) {
            Err(error) => error,
            Ok(_) => panic!("Prepared construction should have been denied"),
        };
        let limit = error
            .downcast_ref::<ResourceLimit>()
            .context("typed Prepared ResourceLimit")?;
        assert_eq!(calls.borrow().as_slice(), [limit.required]);
        assert_eq!(requested.live(), 0);
        Ok(())
    }

    #[test]
    fn prepared_scope_retains_its_graph_through_nested_requested_storage() -> Result<()> {
        let (mut inspection, source) = empty();
        let bytes = b"retained original packet".to_vec();
        inspection.packets.push(xmp_packets::Packet {
            container: xmp_packets::Container::Sidecar,
            blake3: blake3::hash(&bytes).to_hex().to_string(),
            bytes,
            ranges: Vec::new(),
            group: "retained".into(),
            attributes: BTreeMap::new(),
        });
        let calls = RefCell::new(Vec::new());
        let admit = |required| {
            calls.borrow_mut().push(required);
            Ok(())
        };
        let requested = Requested::new(&admit);
        let prepared = Prepared::new_admitted(&inspection, &source, &requested)?;
        let retained = requested.live();
        assert!(retained > 0);
        let nested = requested.scope(17)?;
        assert_eq!(requested.live(), retained + 17);
        drop(nested);
        assert_eq!(requested.live(), retained);
        drop(prepared);
        assert_eq!(requested.live(), 0);
        assert_eq!(calls.borrow().last().copied(), Some(retained + 17));
        Ok(())
    }

    #[test]
    fn nested_resource_limit_is_not_recorded_as_an_xmp_parse_observation() -> Result<()> {
        let (mut inspection, source) = empty();
        inspection.parse_inputs.push(xmp_packets::ParseInput {
            bytes: XMP.to_vec(),
            blake3: blake3::hash(XMP).to_hex().to_string(),
            packet_indices: Vec::new(),
            transformation: xmp_packets::Transformation::Identity,
            group: "xmp".into(),
        });
        let calls = RefCell::new(0usize);
        let admit = |required| {
            let mut calls = calls.borrow_mut();
            *calls += 1;
            if *calls == 1 {
                Ok(())
            } else {
                Err(ResourceLimit {
                    required,
                    available: required.saturating_sub(1),
                }
                .into())
            }
        };
        let requested = Requested::new(&admit);
        let error = match Prepared::new_admitted(&inspection, &source, &requested) {
            Err(error) => error,
            Ok(_) => panic!("nested XMP admission should have been denied"),
        };
        assert!(error.downcast_ref::<ResourceLimit>().is_some());
        assert!(*calls.borrow() >= 2);
        Ok(())
    }

    #[test]
    fn streamed_revision_identity_matches_previous_json_value_digest() -> Result<()> {
        let (inspection, source) = empty();
        let prepared = Prepared::new(&inspection, &source)?;
        let parsed: serde_json::Value = serde_json::from_str(&prepared.provenance)?;
        let source_revision = parsed
            .get("file_revision")
            .context("missing source revision")?;
        let identity = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "source_revision": source_revision,
            "status": prepared.status,
            "issues": prepared.issues,
            "packets": prepared.packets,
            "models": prepared.models.iter().map(|model| (
                &model.hash,
                &model.descriptor,
                &model.projection,
                &model.error,
            )).collect::<Vec<_>>(),
            "provenance": prepared.provenance,
        }))?;
        assert_eq!(prepared.revision, blake3::hash(&identity).to_hex().as_str());
        Ok(())
    }
}
pub(crate) fn revision(db: &Connection, asset: &str) -> Result<i64> {
    let found: Option<i64> = db.query_row("SELECT COALESCE(m.revision,0) FROM image_assets a LEFT JOIN metadata_assets m ON m.asset_id=a.id WHERE a.id=?1",[asset],|r|r.get(0)).optional()?;
    found.context("asset not found")
}
pub(crate) fn advance(
    db: &Connection,
    asset: &str,
    action: &str,
    detail: &serde_json::Value,
    affects_pixels: bool,
) -> Result<i64> {
    db.execute("INSERT INTO metadata_assets(asset_id,revision) VALUES(?1,1) ON CONFLICT(asset_id) DO UPDATE SET revision=revision+1",[asset])?;
    let next = revision(db, asset)?;
    db.execute(
        "INSERT INTO metadata_history(asset_id,revision,action,detail) VALUES(?1,?2,?3,?4)",
        params![asset, next, action, serde_json::to_string(detail)?],
    )?;
    if affects_pixels {
        db.execute(
            "UPDATE catalog_images SET pixel_generation=pixel_generation+1 WHERE id=?",
            [asset],
        )?;
        db.execute(
            "UPDATE assets SET render_generation=render_generation+1 WHERE id=?1",
            [asset],
        )?;
    }
    crate::organization::refresh(db, asset)?;
    Ok(next)
}
fn store(
    db: &Transaction<'_>,
    asset: &str,
    source: &Source,
    prepared: &Prepared,
) -> Result<(i64, Vec<i64>, bool)> {
    revision(db, asset)?;
    let physical = crate::catalog_images::physical(db, asset)?;
    let logical_locator = source.locator.clone();
    let mut scoped_source = source.clone();
    if physical != asset && source.kind == "catalog" {
        scoped_source.locator = [format!("image:{asset}\0").as_bytes(), &source.locator].concat();
    }
    let source = &scoped_source;
    ensure!(
        !source.kind.is_empty() && source.kind.len() <= 100 && source.locator.len() <= 32768,
        "invalid metadata source"
    );
    let association = if source.ambiguous {
        "ambiguous"
    } else {
        "confirmed"
    };
    let old: Option<(i64,Option<i64>,String,String)> = db.query_row("SELECT id,current_observation,association,availability FROM metadata_sources WHERE asset_id=?1 AND kind=?2 AND locator=?3",params![physical,source.kind,source.locator],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    db.execute("INSERT INTO metadata_sources(asset_id,kind,locator,display,association,availability) VALUES(?1,?2,?3,?4,?5,'available') ON CONFLICT(asset_id,kind,locator) DO UPDATE SET display=excluded.display,association=excluded.association,availability='available'",params![physical,source.kind,source.locator,source.display,association])?;
    let sid: i64 = db.query_row(
        "SELECT id FROM metadata_sources WHERE asset_id=?1 AND kind=?2 AND locator=?3",
        params![physical, source.kind, source.locator],
        |r| r.get(0),
    )?;
    let image_old: Option<(Option<i64>, String, String)> = db.query_row("SELECT current_observation,association,availability FROM metadata_image_sources WHERE image_id=?1 AND source_id=?2",params![asset,sid],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let mut observed: Option<i64> = db
        .query_row(
            "SELECT id FROM metadata_observations WHERE source_id=?1 AND revision=?2",
            params![sid, prepared.revision],
            |r| r.get(0),
        )
        .optional()?;
    if observed.is_none()
        && let Some((_, Some(current), _, _)) = &old
        && prepared.matches_relocated_observation(db, *current)?
    {
        observed = Some(*current);
        let hash = blake3::hash(prepared.provenance.as_bytes())
            .to_hex()
            .to_string();
        db.execute("INSERT OR IGNORE INTO metadata_file_instances(asset_id,source_id,evidence_hash,provenance) VALUES(?1,?2,?3,?4)",
            params![physical, sid, hash, prepared.provenance])?;
    }
    let oid = if let Some(id) = observed {
        id
    } else {
        for (hash, (length, compressed)) in &prepared.blobs {
            db.execute(
                "INSERT OR IGNORE INTO metadata_blobs(hash,raw_length,compressed) VALUES(?1,?2,?3)",
                params![hash, *length as i64, compressed],
            )?;
        }
        db.execute("INSERT INTO metadata_observations(source_id,revision,status,issues,provenance) VALUES(?1,?2,?3,?4,?5)",params![sid,prepared.revision,prepared.status,prepared.issues,prepared.provenance])?;
        let oid = db.last_insert_rowid();
        for (i, (hash, descriptor)) in prepared.packets.iter().enumerate() {
            db.execute(
                "INSERT INTO metadata_packets VALUES(?1,?2,?3,?4)",
                params![oid, i as i64, hash, descriptor],
            )?;
        }
        for (i, m) in prepared.models.iter().enumerate() {
            db.execute("INSERT INTO metadata_models(observation_id,ordinal,blob_hash,descriptor,projection,error) VALUES(?1,?2,?3,?4,?5,?6)",params![oid,i as i64,m.hash,m.descriptor,serde_json::to_string(&m.projection)?,m.error])?;
            let mid = db.last_insert_rowid();
            for (field, value) in &m.projection.fields {
                db.execute(
                    "INSERT INTO metadata_values VALUES(?1,?2,?3,?4)",
                    params![
                        mid,
                        field,
                        serde_json::to_string(value)?,
                        m.semantics
                            .get(field)
                            .context("missing field semantic identity")?
                    ],
                )?;
            }
        }
        oid
    };
    if prepared.status == "SourceChanged" {
        db.execute("UPDATE metadata_sources SET availability='source changed during inspection; observation retained for review' WHERE id=?1",[sid])?;
    } else {
        db.execute(
            "UPDATE metadata_sources SET current_observation=?1 WHERE id=?2",
            params![oid, sid],
        )?;
    }
    let ids = db
        .prepare("SELECT id FROM metadata_models WHERE observation_id=?1 ORDER BY ordinal")?
        .query_map([oid], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    db.execute(
        "INSERT OR IGNORE INTO metadata_image_observations VALUES(?1,?2)",
        params![asset, oid],
    )?;
    let current = if prepared.status == "SourceChanged" {
        image_old.as_ref().and_then(|v| v.0)
    } else {
        Some(oid)
    };
    let availability = if prepared.status == "SourceChanged" {
        "source changed during inspection; observation retained for review"
    } else {
        "available"
    };
    db.execute("INSERT INTO metadata_image_sources VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(image_id,source_id) DO UPDATE SET current_observation=excluded.current_observation,association=excluded.association,availability=excluded.availability",params![asset,sid,current,matches!(source.kind.as_str(),"embedded"|"sidecar"),logical_locator,association,availability])?;
    let changed = image_old.is_none_or(|(previous, a, available)| {
        previous != Some(oid) || a != association || available != "available"
    });
    Ok((oid, ids, changed))
}
fn candidates(db: &Connection, asset: &str) -> Result<BTreeMap<String, Vec<Candidate>>> {
    let mut result: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    let mut stmt=db.prepare("SELECT v.field,v.value,m.id,s.id,s.kind,s.display,m.observation_id,CASE WHEN s.association='ambiguous' OR o.status!='Complete' THEN 'ambiguous' ELSE 'confirmed' END,v.semantic_hash FROM image_metadata_sources s JOIN metadata_observations o ON o.id=s.current_observation JOIN metadata_models m ON m.observation_id=s.current_observation JOIN metadata_values v ON v.model_id=m.id WHERE s.asset_id=?1 ORDER BY v.field,s.id,m.ordinal")?;
    let rows = stmt.query_map([asset], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, String>(8)?,
        ))
    })?;
    for row in rows {
        let (
            field,
            value,
            model_id,
            source_id,
            source_kind,
            source_display,
            observation_id,
            association,
            semantic_hash,
        ) = row?;
        result.entry(field).or_default().push(Candidate {
            model_id,
            source_id,
            source_kind,
            source_display,
            observation_id,
            ambiguous: association == "ambiguous",
            value: serde_json::from_str(&value)?,
            semantic_hash,
        });
    }
    Ok(result)
}
pub(crate) fn rebuild(db: &Connection, asset: &str) -> Result<()> {
    let candidates = candidates(db, asset)?;
    let choices = db
        .prepare("SELECT field,model_id FROM metadata_choices WHERE asset_id=?1")?
        .query_map([asset], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    db.execute("DELETE FROM metadata_effective WHERE asset_id=?1", [asset])?;
    let fields = candidates
        .keys()
        .chain(choices.keys())
        .collect::<BTreeSet<_>>();
    for field in fields {
        let values = candidates.get(field).map(Vec::as_slice).unwrap_or_default();
        let chosen = choices.get(field);
        let selected = chosen.and_then(|mid| values.iter().find(|c| c.model_id == *mid));
        let consensus = values.first().filter(|first| {
            values.iter().all(|c| {
                !c.ambiguous && c.value == first.value && c.semantic_hash == first.semantic_hash
            })
        });
        let selected = if chosen.is_some() {
            selected
        } else {
            consensus
        };
        let conflict = selected.is_none() && (!values.is_empty() || chosen.is_some());
        db.execute(
            "INSERT INTO metadata_effective VALUES(?1,?2,?3,?4,?5)",
            params![
                asset,
                field,
                selected
                    .map(|c| serde_json::to_string(&c.value))
                    .transpose()?,
                conflict,
                selected.map(|c| c.model_id)
            ],
        )?;
    }
    Ok(())
}
pub(crate) fn read_blob(db: &Connection, hash: &str) -> Result<Vec<u8>> {
    // Admit both sizes in the same SQLite read before rusqlite allocates the
    // compressed bytes. This matches the inspection bridge's retained-blob bound.
    let (length, data): (i64, Option<Vec<u8>>) = db.query_row(
        "SELECT raw_length,CASE WHEN raw_length BETWEEN 0 AND ?2 AND length(compressed)<=?3 THEN compressed END FROM metadata_blobs WHERE hash=?1",
        params![hash, xmp::MAX_PACKET_BYTES as i64, (xmp::MAX_PACKET_BYTES + 65536) as i64],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        (0..=xmp::MAX_PACKET_BYTES as i64).contains(&length),
        "invalid retained blob size"
    );
    let data = data.context("invalid retained compressed blob size")?;
    let mut bytes = Vec::with_capacity(length as usize);
    ZlibDecoder::new(data.as_slice())
        .take(length as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as i64 == length && blake3::hash(&bytes).to_hex().as_str() == hash,
        "retained metadata checksum mismatch"
    );
    Ok(bytes)
}

#[cfg(test)]
mod retained_blob_admission_tests {
    use super::*;

    #[test]
    fn compressed_size_rejects_before_decoding_even_with_small_raw_claim() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE metadata_blobs(hash TEXT PRIMARY KEY,raw_length INTEGER,compressed BLOB);")?;
        db.execute(
            "INSERT INTO metadata_blobs VALUES('oversized',1,zeroblob(?1))",
            [(xmp::MAX_PACKET_BYTES + 65537) as i64],
        )?;
        assert_eq!(
            read_blob(&db, "oversized").unwrap_err().to_string(),
            "invalid retained compressed blob size"
        );
        for length in [-1, xmp::MAX_PACKET_BYTES as i64 + 1] {
            db.execute("UPDATE metadata_blobs SET raw_length=?1", [length])?;
            assert_eq!(
                read_blob(&db, "oversized").unwrap_err().to_string(),
                "invalid retained blob size"
            );
        }
        Ok(())
    }

    #[test]
    fn admitted_blob_still_requires_exact_raw_length_and_digest() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE metadata_blobs(hash TEXT PRIMARY KEY,raw_length INTEGER,compressed BLOB);")?;
        let bytes = b"retained exact packet bytes\0\xff";
        let hash = blake3::hash(bytes).to_hex().to_string();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes)?;
        db.execute(
            "INSERT INTO metadata_blobs VALUES(?1,?2,?3)",
            params![hash, bytes.len() as i64, encoder.finish()?],
        )?;
        assert_eq!(read_blob(&db, &hash)?, bytes);
        for length in [bytes.len() as i64 - 1, bytes.len() as i64 + 1] {
            db.execute("UPDATE metadata_blobs SET raw_length=?1", [length])?;
            assert_eq!(
                read_blob(&db, &hash).unwrap_err().to_string(),
                "retained metadata checksum mismatch"
            );
        }
        db.execute(
            "UPDATE metadata_blobs SET raw_length=?1,hash='wrong-digest'",
            [bytes.len() as i64],
        )?;
        assert_eq!(
            read_blob(&db, "wrong-digest").unwrap_err().to_string(),
            "retained metadata checksum mismatch"
        );
        Ok(())
    }
}
/// Commit a prepared metadata observation within the caller's writer transaction.
/// Preparation parses and compresses outside the writer; migration proof and its
/// checkpoint can therefore commit atomically with native metadata/recipe state.
pub(crate) fn retain_prepared(
    tx: &Transaction<'_>,
    asset: &str,
    source: &Source,
    prepared: &Prepared,
    shared: bool,
) -> Result<Change> {
    if !shared {
        crate::catalog_images::require_current(tx, asset)?;
    }
    let (observation_id, model_ids, changed) = store(tx, asset, source, prepared)?;
    let revision = if changed {
        rebuild(tx, asset)?;
        advance(
            tx,
            asset,
            "observe",
            &serde_json::json!({"observation_id":observation_id}),
            true,
        )?
    } else {
        revision(tx, asset)?
    };
    if changed && shared {
        crate::catalog_images::enqueue_shared(tx, asset, observation_id)?;
        crate::catalog_images::step_refresh(tx, 32)?;
    }
    Ok(Change {
        revision,
        observation_id,
        model_ids,
        changed,
    })
}

impl Catalog {
    pub fn metadata_file_instances(
        &self,
        asset: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<FileInstance>> {
        ensure!((1..=1000).contains(&limit), "page limit must be 1..1000");
        revision(&self.db, asset)?;
        let mut stmt = self.db.prepare("SELECT id,source_id,provenance,observed_at FROM metadata_file_instances WHERE asset_id=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
        let mut result = Vec::new();
        for row in stmt.query_map(params![asset, after, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?, r.get(3)?))
        })? {
            let (id, source_id, provenance, observed_at) = row?;
            result.push(FileInstance {
                id,
                source_id,
                provenance: serde_json::from_str(&provenance)?,
                observed_at,
            });
        }
        Ok(result)
    }
    /// Hold catalog generation authority only for the final preview-manifest CAS.
    /// Stage and flush image bytes before entering this guard; callbacks must not
    /// acquire catalog locks in reverse order. Visible reads still check current keys.
    /// Metadata revisions also track organization; generation advances for every
    /// potentially pixel-affecting metadata transition, so flags/ratings need not
    /// cancel otherwise valid preview publication. Export keeps its full revision CAS.
    pub fn with_render_identity<T>(
        &mut self,
        expected: &RenderIdentity,
        attach: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        self.with_render_identity_priority(
            expected,
            crate::catalog_writer::Priority::Foreground,
            attach,
        )
    }
    pub(crate) fn with_render_identity_priority<T>(
        &mut self,
        expected: &RenderIdentity,
        priority: crate::catalog_writer::Priority,
        attach: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        self.with_render_transaction(expected, priority, |_| attach())
    }
    pub(crate) fn with_render_transaction<T>(
        &mut self,
        expected: &RenderIdentity,
        priority: crate::catalog_writer::Priority,
        attach: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<Option<T>> {
        let _write = self.writers.enter(priority)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current=tx.query_row("SELECT a.render_generation,a.fingerprint,a.state,COALESCE(m.revision,0) FROM assets a LEFT JOIN metadata_assets m ON m.asset_id=a.id WHERE a.id=?1",[&expected.asset_id],|r|Ok(RenderIdentity{asset_id:expected.asset_id.clone(),generation:r.get(0)?,fingerprint:crate::catalog_row::optional_text(r,1,64)?,state:crate::catalog_row::owned_text(r,2,7)?,metadata_revision:r.get(3)?})).optional()?;
        if !current.as_ref().is_some_and(|current| {
            current.asset_id == expected.asset_id
                && current.generation == expected.generation
                && current.fingerprint == expected.fingerprint
                && current.state == expected.state
        }) {
            return Ok(None);
        }
        let result = attach(&tx)?;
        tx.commit()?;
        drop(_write);
        Ok(Some(result))
    }
    pub fn render_identity(&self, asset: &str) -> Result<RenderIdentity> {
        let (generation, fingerprint, state, metadata_revision) = self.db.query_row(
            "SELECT a.render_generation,a.fingerprint,a.state,COALESCE(m.revision,0) FROM assets a LEFT JOIN metadata_assets m ON m.asset_id=a.id WHERE a.id=?1",
            [asset],
            |r| Ok((r.get(0)?, crate::catalog_row::optional_text(r,1,64)?, crate::catalog_row::owned_text(r,2,7)?,r.get(3)?)),
        )?;
        Ok(RenderIdentity {
            asset_id: asset.into(),
            generation,
            fingerprint,
            state,
            metadata_revision,
        })
    }
    /// Store an external source observation atomically. Never writes to the source.
    pub fn retain_metadata(
        &mut self,
        asset: &str,
        source: &Source,
        inspection: &Inspection,
    ) -> Result<Change> {
        self.retain_image_metadata_id(
            asset,
            source,
            inspection,
            matches!(source.kind.as_str(), "embedded" | "sidecar"),
        )
    }
    pub(crate) fn retain_image_metadata_id(
        &mut self,
        asset: &str,
        source: &Source,
        inspection: &Inspection,
        shared: bool,
    ) -> Result<Change> {
        let prepared = Prepared::new(inspection, source)?;
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = retain_prepared(&tx, asset, source, &prepared, shared)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn metadata(&self, asset: &str) -> Result<MetadataView> {
        let tx = self.db.unchecked_transaction()?;
        crate::catalog_images::require_current(&tx, asset)?;
        let revision = revision(&tx, asset)?;
        let mut candidates = candidates(&tx, asset)?;
        let mut fields = Vec::new();
        let mut stmt=tx.prepare("SELECT field,value,conflicted,model_id FROM metadata_effective WHERE asset_id=?1 ORDER BY field")?;
        for row in stmt.query_map([asset], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })? {
            let (name, value, conflicted, selected_model) = row?;
            fields.push(Field {
                candidates: candidates.remove(&name).unwrap_or_default(),
                name,
                value: value.map(|v| serde_json::from_str(&v)).transpose()?,
                conflicted,
                selected_model,
            });
        }
        let mut stmt=tx.prepare("SELECT s.id,s.kind,s.display,s.association,s.availability,s.current_observation,o.status,o.issues FROM image_metadata_sources s LEFT JOIN metadata_observations o ON o.id=s.current_observation WHERE s.asset_id=?1 ORDER BY s.id")?;
        let mut sources = Vec::new();
        for row in stmt.query_map([asset], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })? {
            let (id, kind, display, association, availability, observation_id, status, issues) =
                row?;
            sources.push(SourceState {
                id,
                kind,
                display,
                association,
                availability,
                observation_id,
                status,
                issues: issues.map(|v| serde_json::from_str(&v)).transpose()?,
            });
        }
        Ok(MetadataView {
            asset_id: asset.into(),
            revision,
            sources,
            fields,
        })
    }
    /// Decisions bind a source revision. If that source changes, the choice becomes visibly stale.
    pub fn resolve_metadata(
        &mut self,
        asset: &str,
        expected_revision: i64,
        field: &str,
        model_id: i64,
    ) -> Result<i64> {
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        crate::catalog_images::require_current(&tx, asset)?;
        ensure!(
            revision(&tx, asset)? == expected_revision,
            "metadata changed; refresh conflict review"
        );
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM image_metadata_sources s JOIN metadata_models m ON m.observation_id=s.current_observation JOIN metadata_values v ON v.model_id=m.id WHERE s.asset_id=?1 AND m.id=?2 AND v.field=?3)",params![asset,model_id,field],|r|r.get(0))?;
        ensure!(
            valid,
            "selection is not a current field candidate for this asset"
        );
        tx.execute("INSERT INTO metadata_choices VALUES(?1,?2,?3) ON CONFLICT(asset_id,field) DO UPDATE SET model_id=excluded.model_id",params![asset,field,model_id])?;
        rebuild(&tx, asset)?;
        let next = advance(
            &tx,
            asset,
            "resolve",
            &serde_json::json!({"field":field,"model_id":model_id}),
            true,
        )?;
        tx.commit()?;
        drop(_write);
        Ok(next)
    }
    pub(crate) fn resolve_metadata_for_image_with_receipt(
        &mut self,
        identity: &crate::catalog_images::ImageMetadataIdentity,
        field: &str,
        model_id: i64,
        attempt: &str,
        request_digest: &str,
    ) -> Result<i64> {
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(
            crate::catalog_metadata_write::existing(&tx, attempt, request_digest)?.is_none(),
            "metadata attempt already committed"
        );
        crate::catalog_images::require_image_metadata_identity(&tx, identity)?;
        let asset = identity.image_id.as_str();
        ensure!(
            revision(&tx, asset)? == identity.metadata_revision,
            "metadata changed; refresh conflict review"
        );
        let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM image_metadata_sources s JOIN metadata_models m ON m.observation_id=s.current_observation JOIN metadata_values v ON v.model_id=m.id WHERE s.asset_id=?1 AND m.id=?2 AND v.field=?3)", params![asset, model_id, field], |row| row.get(0))?;
        ensure!(
            valid,
            "selection is not a current field candidate for this image"
        );
        tx.execute("INSERT INTO metadata_choices VALUES(?1,?2,?3) ON CONFLICT(asset_id,field) DO UPDATE SET model_id=excluded.model_id", params![asset, field, model_id])?;
        rebuild(&tx, asset)?;
        let next = advance(
            &tx,
            asset,
            "resolve",
            &serde_json::json!({"field":field,"model_id":model_id}),
            true,
        )?;
        crate::catalog_metadata_write::insert(
            &tx,
            attempt,
            request_digest,
            "resolve",
            &crate::catalog_metadata_write::Owner::Image {
                identity: identity.clone(),
            },
            &serde_json::json!({"revision": next}),
        )?;
        tx.commit()?;
        Ok(next)
    }
    pub fn metadata_model(&self, asset: &str, model_id: i64) -> Result<Vec<u8>> {
        let hash:String=self.db.query_row("SELECT m.blob_hash FROM metadata_models m JOIN metadata_observations o ON o.id=m.observation_id JOIN metadata_image_observations h ON h.observation_id=o.id WHERE h.image_id=?1 AND m.id=?2",params![asset,model_id],|r|r.get(0))?;
        read_blob(&self.db, &hash)
    }
    fn editable_metadata_model(&self, asset: &str, model_id: i64) -> Result<Vec<u8>> {
        let bytes = self.metadata_model(asset, model_id)?;
        let error: Option<String> = self.db.query_row(
            "SELECT error FROM metadata_models WHERE id=?1",
            [model_id],
            |r| r.get(0),
        )?;
        ensure!(
            error.is_none(),
            "model cannot be edited or exported directly: {}",
            error.unwrap_or_default()
        );
        xmp::parse(&bytes)?;
        Ok(bytes)
    }
    pub fn metadata_history(
        &self,
        asset: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<Observation>> {
        revision(&self.db, asset)?;
        ensure!(
            after >= 0 && (1..=100).contains(&limit),
            "history requires nonnegative cursor and limit 1..=100"
        );
        let mut result = Vec::new();
        let mut stmt=self.db.prepare("SELECT o.id,o.source_id,o.revision,o.status,o.issues,o.provenance,COALESCE(s.current_observation=o.id,0) FROM metadata_observations o JOIN metadata_image_observations h ON h.observation_id=o.id LEFT JOIN image_metadata_sources s ON s.id=o.source_id AND s.asset_id=h.image_id WHERE h.image_id=?1 AND o.id>?2 ORDER BY o.id LIMIT ?3")?;
        for row in stmt.query_map(params![asset, after, limit as i64], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get(6)?,
            ))
        })? {
            let (id, source_id, revision, status, issues, provenance, current) = row?;
            let mut models = Vec::new();
            for row in self.db.prepare("SELECT id,ordinal,blob_hash,descriptor,projection,error FROM metadata_models WHERE observation_id=?1 ORDER BY ordinal")?.query_map([id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get(5)?)))? {
                let (id,ordinal,blob_hash,descriptor,projection,error)=row?;models.push(ModelSummary{id,ordinal,blob_hash,descriptor:serde_json::from_str(&descriptor)?,projection:serde_json::from_str(&projection)?,error});
            }
            result.push(Observation {
                id,
                source_id,
                revision,
                status,
                issues: serde_json::from_str(&issues)?,
                provenance: serde_json::from_str(&provenance)?,
                current,
                models,
            });
        }
        Ok(result)
    }
    pub fn metadata_packets(
        &self,
        asset: &str,
        observation_id: i64,
    ) -> Result<Vec<PacketEvidence>> {
        let valid:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM metadata_image_observations h WHERE h.image_id=?1 AND h.observation_id=?2)",params![asset,observation_id],|r|r.get(0))?;
        ensure!(valid, "observation not owned by asset");
        let mut output = Vec::new();
        for row in self.db.prepare("SELECT ordinal,blob_hash,descriptor FROM metadata_packets WHERE observation_id=?1 ORDER BY ordinal")?.query_map([observation_id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))? {
            let (ordinal,blake3,descriptor)=row?;output.push(PacketEvidence{ordinal,bytes:read_blob(&self.db,&blake3)?,blake3,descriptor:serde_json::from_str(&descriptor)?});
        }
        Ok(output)
    }
    /// Create a new catalog-owned model. Existing observations and original files remain intact.
    pub fn edit_metadata(
        &mut self,
        asset: &str,
        expected_revision: i64,
        base_model: Option<i64>,
        edits: &[Edit],
    ) -> Result<Change> {
        self.edit_metadata_commit(asset, expected_revision, base_model, edits, &[], |_, _| {
            Ok(())
        })
    }
    pub(crate) fn organization_edit_input(
        &self,
        asset: &str,
        base_model: Option<i64>,
        organization_fields: &[String],
    ) -> Result<Vec<u8>> {
        let mut input = base_model
            .map(|id| self.editable_metadata_model(asset, id))
            .transpose()?
            .unwrap_or(xmp::empty_packet()?);
        if !organization_fields.is_empty() {
            let view = self.metadata(asset)?;
            let mut fields = Vec::new();
            for field in view
                .fields
                .iter()
                .filter(|f| organization_fields.contains(&f.name))
            {
                ensure!(
                    !field.conflicted,
                    "resolve {} metadata conflict before organization edits",
                    field.name
                );
                let bytes = if field.value == Some(Value::Removed) {
                    None
                } else {
                    Some(
                        self.editable_metadata_model(
                            asset,
                            field
                                .selected_model
                                .context("field has no selected model")?,
                        )?,
                    )
                };
                fields.push((field.name.clone(), bytes));
            }
            input = xmp::reconcile_fields(&input, &fields)?;
        }
        Ok(input)
    }
    pub(crate) fn edit_metadata_commit(
        &mut self,
        asset: &str,
        expected_revision: i64,
        base_model: Option<i64>,
        edits: &[Edit],
        organization_fields: &[String],
        after: impl FnOnce(&Connection, i64) -> Result<()>,
    ) -> Result<Change> {
        let prepared = self.prepare_metadata_edit(
            asset,
            expected_revision,
            base_model,
            edits,
            organization_fields,
        )?;
        self.commit_prepared_metadata_edit(prepared, after)
    }

    /// Parse, project and compress on the selected catalog worker, without a
    /// writer permit. The returned value cannot authorize a different session.
    pub(crate) fn prepare_metadata_edit(
        &self,
        asset: &str,
        expected_revision: i64,
        base_model: Option<i64>,
        edits: &[Edit],
        organization_fields: &[String],
    ) -> Result<PreparedEdit> {
        let image_identity = crate::catalog_images::identity(&self.db, asset)?;
        ensure!(
            image_identity.metadata_revision == expected_revision,
            "metadata changed; refresh before editing"
        );
        ensure!(
            organization_fields.iter().all(|f| matches!(
                f.as_str(),
                "rating" | "label" | "keywords" | "hierarchical_keywords"
            )),
            "organization field can affect pixels"
        );
        let input = self.organization_edit_input(asset, base_model, organization_fields)?;
        let bytes = if organization_fields.is_empty() {
            xmp::apply_edits(&input, edits)?
        } else {
            xmp::apply_organization_edits(&input, edits)?
        };
        let original = if organization_fields.is_empty()
            && let Some(mid) = base_model
        {
            let json: String = self.db.query_row(
                "SELECT projection FROM metadata_models WHERE id=?1",
                [mid],
                |r| r.get(0),
            )?;
            serde_json::from_str::<Projection>(&json)?
        } else {
            xmp::project(&input)?
        };
        let original_semantics = if organization_fields.is_empty()
            && let Some(mid) = base_model
        {
            self.db
                .prepare("SELECT field,semantic_hash FROM metadata_values WHERE model_id=?1")?
                .query_map([mid], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()?
        } else {
            xmp::field_semantics(&input)?
        };
        let mut updated = xmp::project(&bytes)?;
        for field in original.fields.keys() {
            if !updated.fields.contains_key(field) {
                updated.fields.insert(field.clone(), Value::Removed);
            }
        }
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let source = Source {
            kind: "catalog".into(),
            locator: b"local-metadata".to_vec(),
            display: "Catalog metadata".into(),
            ambiguous: false,
            provenance: serde_json::json!({"base_model":base_model,"edits":edits,"based_on_revision":expected_revision}),
        };
        let inspection = Inspection {
            revision: xmp_packets::SourceRevision {
                length: bytes.len() as u64,
                blake3: hash.clone(),
                modified_unix_ns: None,
            },
            status: Status::Complete,
            issues: vec![],
            packets: vec![xmp_packets::Packet {
                container: xmp_packets::Container::Sidecar,
                bytes: bytes.clone(),
                blake3: hash.clone(),
                ranges: vec![],
                group: "catalog-authored".into(),
                attributes: BTreeMap::new(),
            }],
            parse_inputs: vec![xmp_packets::ParseInput {
                bytes,
                blake3: hash,
                packet_indices: vec![0],
                transformation: xmp_packets::Transformation::Identity,
                group: "catalog-authored".into(),
            }],
        };
        let mut prepared = Prepared::new(&inspection, &source)?;
        prepared.models[0].projection = updated.clone();
        for (field, value) in &updated.fields {
            if *value == Value::Removed {
                prepared.models[0].semantics.insert(
                    field.clone(),
                    blake3::hash(b"catalog-explicit-removal-v1")
                        .to_hex()
                        .to_string(),
                );
            }
        }
        prepared.revision = blake3::hash(&serde_json::to_vec(&(&prepared.revision, &updated))?)
            .to_hex()
            .to_string();
        Ok(PreparedEdit {
            catalog_file: self.session.clone(),
            image_identity,
            expected_revision,
            base_model,
            edits: edits.to_vec(),
            organization_fields: organization_fields.to_vec(),
            source,
            prepared,
            original,
            original_semantics,
            updated,
        })
    }

    /// Revalidate the frozen image authority and publish the prepared edit in
    /// the same transaction as any organization job/event bookkeeping.
    pub(crate) fn commit_prepared_metadata_edit(
        &mut self,
        edit: PreparedEdit,
        after: impl FnOnce(&Connection, i64) -> Result<()>,
    ) -> Result<Change> {
        ensure!(
            std::sync::Arc::ptr_eq(&self.session, &edit.catalog_file),
            "prepared metadata edit belongs to another catalog session"
        );
        let PreparedEdit {
            catalog_file: _,
            image_identity,
            expected_revision,
            base_model,
            edits,
            organization_fields,
            source,
            prepared,
            original,
            original_semantics,
            updated,
        } = edit;
        let asset = image_identity.image_id.as_str();
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        crate::catalog_images::require_image_metadata_identity(&tx, &image_identity)?;
        ensure!(
            revision(&tx, asset)? == expected_revision,
            "metadata changed while preparing edit"
        );
        let previous_choices=tx.prepare("SELECT c.field,v.semantic_hash FROM metadata_choices c JOIN metadata_values v ON v.model_id=c.model_id AND v.field=c.field JOIN metadata_models m ON m.id=c.model_id JOIN metadata_observations o ON o.id=m.observation_id JOIN image_metadata_sources s ON s.id=o.source_id AND s.asset_id=c.asset_id WHERE c.asset_id=?1 AND s.kind='catalog' AND s.locator=?2 AND s.current_observation=o.id")?.query_map(params![asset,source.locator],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<BTreeMap<_,_>>>()?;
        let (observation_id, model_ids, _) = store(&tx, asset, &source, &prepared)?;
        let mid = model_ids[0];
        for (field, value) in &updated.fields {
            if organization_fields.contains(field)
                || original.fields.get(field) != Some(value)
                || original_semantics.get(field) != prepared.models[0].semantics.get(field)
                || previous_choices
                    .get(field)
                    .is_some_and(|old| Some(old) == prepared.models[0].semantics.get(field))
            {
                tx.execute("INSERT INTO metadata_choices VALUES(?1,?2,?3) ON CONFLICT(asset_id,field) DO UPDATE SET model_id=excluded.model_id",params![asset,field,mid])?;
            }
        }
        rebuild(&tx, asset)?;
        let revision = advance(
            &tx,
            asset,
            "edit",
            &serde_json::json!({"observation_id":observation_id,"base_model":base_model,"edits":edits}),
            organization_fields.is_empty(),
        )?;
        after(&tx, revision)?;
        tx.commit()?;
        drop(_write);
        Ok(Change {
            revision,
            observation_id,
            model_ids,
            changed: true,
        })
    }

    pub(crate) fn commit_prepared_metadata_edit_with_receipt(
        &mut self,
        edit: PreparedEdit,
        attempt: &str,
        request_digest: &str,
    ) -> Result<Change> {
        let identity = edit.image_identity.clone();
        self.commit_prepared_metadata_edit(edit, |db, revision| {
            crate::catalog_metadata_write::insert(
                db,
                attempt,
                request_digest,
                "edit",
                &crate::catalog_metadata_write::Owner::Image { identity },
                &serde_json::json!({"revision": revision}),
            )
        })
    }
}

#[cfg(test)]
#[path = "catalog_metadata/prepared_edit_tests.rs"]
mod prepared_edit_tests;

fn name_key(name: &std::ffi::OsStr) -> Vec<u8> {
    if let Some(s) = name.to_str() {
        let mut key = vec![1];
        key.extend_from_slice(s.to_lowercase().as_bytes());
        key
    } else {
        let mut key = vec![0];
        key.extend(
            location_bytes(Path::new(name))
                .into_iter()
                .map(|b| b.to_ascii_lowercase()),
        );
        key
    }
}
pub(crate) fn set_import_source_unavailable(
    tx: &Transaction<'_>,
    asset: &str,
    source: &Source,
    reason: &str,
) -> Result<bool> {
    let previous:Option<String>=tx.query_row("SELECT availability FROM metadata_sources WHERE asset_id=?1 AND kind=?2 AND locator=?3",params![asset,source.kind,source.locator],|r|r.get(0)).optional()?;
    if previous.as_deref() == Some(reason) {
        return Ok(false);
    }
    tx.execute("INSERT INTO metadata_sources(asset_id,kind,locator,display,association,availability) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(asset_id,kind,locator) DO UPDATE SET availability=excluded.availability",params![asset,source.kind,source.locator,source.display,if source.ambiguous {"ambiguous"} else {"confirmed"},reason])?;
    // The foreground master receives this change now; the bounded queue
    // propagates it to followers without advancing the master twice.
    tx.execute("INSERT OR IGNORE INTO metadata_image_observations SELECT ?1,current_observation FROM metadata_sources WHERE asset_id=?1 AND kind=?2 AND locator=?3 AND current_observation IS NOT NULL",params![asset,source.kind,source.locator])?;
    tx.execute("INSERT INTO metadata_image_sources SELECT ?1,id,current_observation,1,locator,association,availability FROM metadata_sources WHERE asset_id=?1 AND kind=?2 AND locator=?3 ON CONFLICT(image_id,source_id) DO UPDATE SET current_observation=excluded.current_observation,logical_locator=excluded.logical_locator,association=excluded.association,availability=excluded.availability",params![asset,source.kind,source.locator])?;
    advance(
        tx,
        asset,
        "source_unavailable",
        &serde_json::json!({"kind":source.kind,"display":source.display,"reason":reason}),
        true,
    )?;
    let source_id: i64 = tx.query_row(
        "SELECT id FROM metadata_sources WHERE asset_id=?1 AND kind=?2 AND locator=?3",
        params![asset, source.kind, source.locator],
        |r| r.get(0),
    )?;
    crate::catalog_images::enqueue_source_state(tx, source_id)?;
    crate::catalog_images::step_refresh(tx, 32)?;
    Ok(true)
}

pub(crate) fn apply_import_source(
    tx: &Transaction<'_>,
    asset: &str,
    input: &PreparedImportSource,
) -> Result<(bool, bool)> {
    let changed = match &input.prepared {
        Ok(prepared) => retain_prepared(tx, asset, &input.source, prepared, true)?.changed,
        Err(reason) => set_import_source_unavailable(tx, asset, &input.source, reason)?,
    };
    crate::catalog_storage::record_metadata_path(
        tx,
        asset,
        &input.source.kind,
        &path_from_bytes(&input.source.locator)?,
    )?;
    Ok((changed, input.warning || input.source.ambiguous))
}
pub(crate) fn finish_import_sources(
    tx: &Transaction<'_>,
    asset: &str,
    seen: &BTreeSet<Vec<u8>>,
) -> Result<bool> {
    let previous=tx.prepare("SELECT locator,display,association FROM metadata_sources WHERE asset_id=?1 AND kind='sidecar'")?.query_map([asset],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut changed = false;
    for (locator, display, association) in previous {
        if !seen.contains(&locator) {
            changed |= set_import_source_unavailable(
                tx,
                asset,
                &Source {
                    kind: "sidecar".into(),
                    locator,
                    display,
                    ambiguous: association == "ambiguous",
                    provenance: serde_json::Value::Null,
                },
                "not found in current directory scan; retained metadata remains available",
            )?;
        }
    }
    Ok(changed)
}

pub(crate) fn initialize_discovery(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TEMP TABLE IF NOT EXISTS metadata_scan_dirs(directory BLOB PRIMARY KEY); CREATE TEMP TABLE IF NOT EXISTS metadata_scan_files(directory BLOB NOT NULL,name BLOB NOT NULL,stem BLOB NOT NULL,full_name BLOB NOT NULL,path BLOB NOT NULL,display TEXT NOT NULL,sidecar INTEGER NOT NULL,PRIMARY KEY(directory,name)); CREATE INDEX IF NOT EXISTS metadata_scan_stem ON metadata_scan_files(directory,stem); CREATE INDEX IF NOT EXISTS metadata_scan_name ON metadata_scan_files(directory,full_name); DELETE FROM metadata_scan_dirs; DELETE FROM metadata_scan_files;")?;
    Ok(())
}
fn index_directory(
    db: &Connection,
    directory: &Path,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<()> {
    let key = location_bytes(directory);
    let indexed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata_scan_dirs WHERE directory=?1)",
        [&key],
        |r| r.get(0),
    )?;
    if indexed {
        return Ok(());
    }
    // Enumeration only opens a directory; regular-file content remains the
    // source reader's responsibility. The SQL seam also consumes bounded facts
    // from an eventual F stream without moving this connection to that owner.
    index_directory_facts(
        db,
        directory,
        fs::read_dir(directory)?.map(|entry| {
            let entry = entry?;
            Ok(DirectoryFact {
                path: entry.path(),
                regular: entry.file_type()?.is_file(),
            })
        }),
        cancel,
    )
}

pub(crate) struct DirectoryFact {
    pub(crate) path: std::path::PathBuf,
    pub(crate) regular: bool,
}
fn index_directory_facts(
    db: &Connection,
    directory: &Path,
    facts: impl IntoIterator<Item = Result<DirectoryFact>>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<()> {
    let key = location_bytes(directory);
    let tx = db.unchecked_transaction()?;
    for (index, fact) in facts.into_iter().enumerate() {
        if let Some(cancel) = cancel {
            ensure!(
                !cancel.load(std::sync::atomic::Ordering::Acquire),
                "metadata discovery canceled"
            );
            ensure!(
                index < xmp_packets::Limits::default().max_entries,
                "directory metadata association entry limit exceeded; no truncated associations admitted"
            );
        }
        let fact = fact?;
        ensure!(
            fact.path.parent() == Some(directory),
            "directory fact belongs to another parent"
        );
        if cancel.is_some() {
            let native = crate::storage_volume::NativePath::from_path(&fact.path);
            let units = match &native {
                crate::storage_volume::NativePath::UnixBytes(v) => v.len(),
                crate::storage_volume::NativePath::WindowsWide(v) => v.len(),
            };
            ensure!(
                units <= crate::catalog_session::PATH_UNITS,
                "directory fact path exceeds byte admission"
            );
            native.to_path()?;
        }
        if !fact.regular {
            continue;
        }
        let path = &fact.path;
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ext != "xmp" && !crate::media::supported_extension(&ext) {
            continue;
        }
        let name = path.file_name().context("source has no filename")?;
        let stem = path.file_stem().context("source has no stem")?;
        tx.execute(
            "INSERT INTO metadata_scan_files VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                key,
                location_bytes(Path::new(name)),
                name_key(stem),
                name_key(name),
                location_bytes(path),
                path.to_string_lossy(),
                ext == "xmp"
            ],
        )?;
    }
    tx.execute("INSERT INTO metadata_scan_dirs VALUES(?1)", [key])?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn initialize_discovery_connection(db: &Connection) -> Result<()> {
    db.execute_batch("PRAGMA temp_store=FILE; PRAGMA cache_size=-8192; PRAGMA mmap_size=0;")?;
    initialize_discovery(db)?;
    db.execute_batch("PRAGMA temp.max_page_count=16384; PRAGMA temp.cache_size=-8192;")?;
    Ok(())
}

/// Worker-owned directory facts; no catalog connection or authority is held.
pub(crate) struct ImportDiscovery {
    db: crate::catalog_session::SqlConnection,
    progress_registered: bool,
}
impl ImportDiscovery {
    pub(crate) fn new() -> Result<Self> {
        let db = Connection::open("")?;
        initialize_discovery_connection(&db)?;
        Ok(Self {
            db: db.into(),
            progress_registered: false,
        })
    }
    pub(crate) fn from_admitted(
        db: crate::catalog_session::SqlConnection,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Self> {
        let mut owner = Self {
            db,
            progress_registered: false,
        };
        owner.db.install_cancel_progress(cancel)?;
        owner.progress_registered = true;
        initialize_discovery_connection(&owner.db)?;
        Ok(owner)
    }
    pub(crate) fn sidecars(
        &self,
        path: &Path,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<Source>> {
        let directory = path.parent().context("original has no parent")?;
        index_directory(&self.db, directory, Some(cancel))?;
        let stem = name_key(path.file_stem().context("original has no stem")?);
        let name = name_key(path.file_name().context("original has no filename")?);
        let rows=self.db.prepare("SELECT path,display,stem FROM metadata_scan_files WHERE directory=?1 AND sidecar=1 AND (stem=?2 OR stem=?3) ORDER BY name LIMIT 1025")?.query_map(params![location_bytes(directory),stem,name],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            rows.len() <= 1024,
            "sidecar association limit exceeded; no associations silently discarded"
        );
        let multiple = rows.len() > 1;
        rows.into_iter().map(|(locator, display, key)| {
            let matches:i64=self.db.query_row("SELECT count(*) FROM metadata_scan_files WHERE directory=?1 AND sidecar=0 AND (stem=?2 OR full_name=?2)",params![location_bytes(directory),key],|r|r.get(0))?;
            Ok(Source {kind:"sidecar".into(), locator, display, ambiguous:multiple || matches!=1,
                provenance:serde_json::json!({"discovery":"case-insensitive stem or full filename plus .xmp","matching_photos":matches,"matching_sidecars":if multiple {"multiple"} else {"one"}})})
        }).collect()
    }
}
impl Drop for ImportDiscovery {
    fn drop(&mut self) {
        if self.db.hook_owner_unverifiable() {
            return;
        }
        if self.progress_registered && self.db.remove_progress_handler().is_err() {
            // Managed roles are always owned connections. A check_owned error
            // violates that invariant: retain the owner without further SQL.
            return;
        }
        if !self.db.is_autocommit() {
            let _ = self.db.execute_batch("ROLLBACK");
        }
    }
}
pub(crate) struct PreparedImportSource {
    pub(crate) source: Source,
    pub(crate) prepared: std::result::Result<Prepared, String>,
    pub(crate) warning: bool,
}
pub(crate) fn prepare_import_source(
    source: Source,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<PreparedImportSource> {
    let path = path_from_bytes(&source.locator)?;
    let inspected = xmp_packets::inspect_cancellable(
        &path,
        &Limits::default(),
        source.kind == "sidecar",
        cancel,
    );
    ensure!(
        !cancel.load(std::sync::atomic::Ordering::Acquire),
        "metadata preparation canceled"
    );
    let (prepared, warning) = match inspected {
        Ok(inspection) => {
            let prepared = Prepared::new(&inspection, &source)?;
            let warning = !matches!(inspection.status, Status::Complete | Status::Absent)
                || prepared
                    .models
                    .iter()
                    .any(|m| m.error.is_some() || !m.projection.issues.is_empty());
            (Ok(prepared), warning)
        }
        Err(e) => (Err(format!("inspection failed: {e}")), true),
    };
    ensure!(
        !cancel.load(std::sync::atomic::Ordering::Acquire),
        "metadata preparation canceled"
    );
    Ok(PreparedImportSource {
        source,
        prepared,
        warning,
    })
}

impl Catalog {
    pub(crate) fn begin_metadata_scan(&self) -> Result<()> {
        initialize_discovery(&self.db)
    }
    fn index_metadata_directory(&self, directory: &Path) -> Result<()> {
        index_directory(&self.db, directory, None)
    }
    fn unavailable_metadata_source(
        &mut self,
        asset: &str,
        source: &Source,
        reason: &str,
    ) -> Result<bool> {
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = set_import_source_unavailable(&tx, asset, source, reason)?;
        tx.commit()?;
        Ok(changed)
    }
    fn inspect_metadata_source(
        &mut self,
        asset: &str,
        path: &Path,
        source: &Source,
        sidecar: bool,
    ) -> Result<(bool, bool)> {
        let inspected = if sidecar {
            xmp_packets::inspect_sidecar(path, &Limits::default())
        } else {
            xmp_packets::inspect(path, &Limits::default())
        };
        match inspected {
            Ok(inspection) => {
                let warning = !matches!(inspection.status, Status::Complete | Status::Absent)
                    || inspection
                        .parse_inputs
                        .iter()
                        .any(|i| xmp::project(&i.bytes).map_or(true, |p| !p.issues.is_empty()));
                let change = self.retain_metadata(asset, source, &inspection)?;
                Ok((change.changed, warning))
            }
            Err(error) => Ok((
                self.unavailable_metadata_source(
                    asset,
                    source,
                    &format!("inspection failed: {error}"),
                )?,
                true,
            )),
        }
    }
    pub(crate) fn refresh_metadata(
        &mut self,
        path: &Path,
        embedded_changed: bool,
    ) -> Result<(bool, usize)> {
        let asset: String = self.db.query_row(
            "SELECT id FROM assets WHERE location=?1",
            [location_bytes(path)],
            |r| r.get(0),
        )?;
        let source = Source {
            kind: "embedded".into(),
            locator: location_bytes(path),
            display: path.to_string_lossy().into_owned(),
            ambiguous: false,
            provenance: serde_json::json!({"discovery":"original file"}),
        };
        let already:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM metadata_sources WHERE asset_id=?1 AND kind='embedded' AND locator=?2 AND current_observation IS NOT NULL AND availability='available')",params![asset,source.locator],|r|r.get(0))?;
        let mut changed = false;
        let mut warnings = 0;
        if embedded_changed || !already {
            let (c, w) = self.inspect_metadata_source(&asset, path, &source, false)?;
            changed |= c;
            warnings += usize::from(w);
        }
        {
            let _write = self
                .writers
                .enter(crate::catalog_writer::Priority::Background)?;
            crate::catalog_storage::record_metadata_path(&self.db, &asset, "embedded", path)?;
        }
        let directory = path.parent().context("original has no parent")?;
        self.index_metadata_directory(directory)?;
        let stem = name_key(path.file_stem().context("original has no stem")?);
        let name = name_key(path.file_name().context("original has no filename")?);
        let sidecars=self.db.prepare("SELECT path,display,stem FROM metadata_scan_files WHERE directory=?1 AND sidecar=1 AND (stem=?2 OR stem=?3) ORDER BY name LIMIT 1025")?.query_map(params![location_bytes(directory),stem,name],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            sidecars.len() <= 1024,
            "sidecar association limit exceeded; no associations silently discarded"
        );
        let mut seen = BTreeSet::new();
        let multiple = sidecars.len() > 1;
        for (locator, display, key) in sidecars {
            let matches:i64=self.db.query_row("SELECT count(*) FROM metadata_scan_files WHERE directory=?1 AND sidecar=0 AND (stem=?2 OR full_name=?2)",params![location_bytes(directory),key],|r|r.get(0))?;
            seen.insert(locator.clone());
            let sidecar = path_from_bytes(&locator)?;
            let source = Source {
                kind: "sidecar".into(),
                locator,
                display,
                ambiguous: multiple || matches != 1,
                provenance: serde_json::json!({"discovery":"case-insensitive stem or full filename plus .xmp","matching_photos":matches,"matching_sidecars":if multiple {"multiple"} else {"one"}}),
            };
            let (c, w) = self.inspect_metadata_source(&asset, &sidecar, &source, true)?;
            {
                let _write = self
                    .writers
                    .enter(crate::catalog_writer::Priority::Background)?;
                crate::catalog_storage::record_metadata_path(
                    &self.db, &asset, "sidecar", &sidecar,
                )?;
            }
            changed |= c;
            warnings += usize::from(w || source.ambiguous);
        }
        let previous=self.db.prepare("SELECT locator,display,association FROM metadata_sources WHERE asset_id=?1 AND kind='sidecar'")?.query_map([&asset],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (locator, display, association) in previous {
            if !seen.contains(&locator) {
                changed |= self.unavailable_metadata_source(
                    &asset,
                    &Source {
                        kind: "sidecar".into(),
                        locator,
                        display,
                        ambiguous: association == "ambiguous",
                        provenance: serde_json::Value::Null,
                    },
                    "not found in current directory scan; retained metadata remains available",
                )?;
            }
        }
        Ok((changed, warnings))
    }
}
#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}
#[cfg(windows)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    ensure!(bytes.len().is_multiple_of(2), "invalid native path");
    Ok(PathBuf::from(std::ffi::OsString::from_wide(
        &bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect::<Vec<_>>(),
    )))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MetadataExportPlan {
    pub asset_id: String,
    pub metadata_revision: i64,
    pub base_model: i64,
    pub destination: crate::metadata_export::ExportPlan,
    pub projected: Projection,
}
impl Catalog {
    /// Prepare a durable reviewable export inside the catalog; destination is only read.
    /// Resolved common fields are copied as complete properties into the chosen full base model.
    pub fn plan_metadata_export(
        &mut self,
        asset: &str,
        expected_revision: i64,
        base_model: i64,
        destination: &Path,
    ) -> Result<MetadataExportPlan> {
        self.plan_metadata_export_with_cancel(
            asset,
            expected_revision,
            base_model,
            destination,
            &std::sync::atomic::AtomicBool::new(false),
            crate::catalog_session::metadata_files::EVIDENCE_BYTES,
            Default::default(),
        )
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Preserve the explicit reviewed authority, limits and cancellation contract at this boundary."
    )]
    pub(crate) fn plan_metadata_export_with_cancel(
        &mut self,
        asset: &str,
        expected_revision: i64,
        base_model: i64,
        destination: &Path,
        cancel: &std::sync::atomic::AtomicBool,
        max_existing_bytes: u64,
        alias_limits: crate::catalog_export_alias::AliasLimits,
    ) -> Result<MetadataExportPlan> {
        self.require_jobs_released()?;
        self.reconcile_export_paths(512)?;
        ensure!(max_existing_bytes > 0, "existing file byte limit");
        alias_limits.validate()?;
        let (payload, projected) =
            self.resolved_export_xmp(asset, expected_revision, base_model)?;
        let native = crate::storage_volume::NativePath::from_path(destination);
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(
            revision(&tx, asset)? == expected_revision,
            "metadata changed during export planning"
        );
        let mut control = crate::catalog_exports::ExportControl::new(cancel);
        crate::catalog_exports::protect_catalog_original_destination_controlled(
            &tx,
            &self.session,
            destination,
            alias_limits,
            &mut control,
        )?;
        let plan = match self.session.plan_metadata_file(
            &native,
            &payload,
            max_existing_bytes,
            alias_limits,
            cancel,
        )? {
            Some(plan) => plan,
            None => {
                let mut checkpoint = |_| {
                    if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "metadata planning canceled",
                        ))
                    } else {
                        Ok(())
                    }
                };
                crate::metadata_export::plan_export_controlled(
                    destination,
                    &payload,
                    max_existing_bytes,
                    alias_limits,
                    &mut checkpoint,
                )?
            }
        };
        let hash = blake3::hash(&payload).to_hex().to_string();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&payload)?;
        let compressed = encoder.finish()?;
        tx.execute(
            "INSERT OR IGNORE INTO metadata_blobs VALUES(?1,?2,?3)",
            params![hash, payload.len() as i64, compressed],
        )?;
        tx.execute(
            "INSERT INTO metadata_export_plans VALUES(?1,?2,?3,?4,?5,?6,NULL)",
            params![
                plan.operation,
                asset,
                expected_revision,
                base_model,
                serde_json::to_string(&plan)?,
                hash
            ],
        )?;
        tx.commit()?;
        drop(_write);
        Ok(MetadataExportPlan {
            asset_id: asset.into(),
            metadata_revision: expected_revision,
            base_model,
            destination: plan,
            projected,
        })
    }
    /// Immutable selected full XMP base with resolved common fields. Native photo
    /// export freezes these bytes separately from its later derivative transforms.
    pub(crate) fn resolved_export_xmp(
        &self,
        asset: &str,
        expected_revision: i64,
        base_model: i64,
    ) -> Result<(Vec<u8>, Projection)> {
        let view = self.metadata(asset)?;
        ensure!(
            view.revision == expected_revision,
            "metadata changed before export planning"
        );
        ensure!(
            view.fields.iter().all(|f| !f.conflicted),
            "resolve metadata conflicts before export"
        );
        let base = self.editable_metadata_model(asset, base_model)?;
        let mut fields = Vec::new();
        for field in &view.fields {
            let value = field
                .value
                .as_ref()
                .context("effective metadata field has no value")?;
            let bytes = if *value == Value::Removed {
                None
            } else {
                Some(self.editable_metadata_model(
                    asset,
                    field.selected_model.context("field has no source model")?,
                )?)
            };
            fields.push((field.name.clone(), bytes));
        }
        let payload = xmp::reconcile_fields(&base, &fields)?;
        let projected = xmp::project(&payload)?;
        for field in &view.fields {
            let actual = projected.fields.get(&field.name);
            ensure!(
                if field.value == Some(Value::Removed) {
                    actual.is_none()
                } else {
                    actual == field.value.as_ref()
                },
                "export does not match resolved field {}",
                field.name
            );
        }
        ensure!(
            self.metadata(asset)?.revision == expected_revision,
            "metadata changed during export snapshot"
        );
        Ok((payload, projected))
    }

    /// Publish exactly the planned bytes, with catalog revision and destination revision checks.
    pub fn apply_metadata_export(
        &mut self,
        operation: &str,
    ) -> Result<crate::metadata_export::ExportReceipt> {
        self.apply_metadata_export_with_cancel(
            operation,
            &std::sync::atomic::AtomicBool::new(false),
        )
    }
    pub(crate) fn apply_metadata_export_with_cancel(
        &mut self,
        operation: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::metadata_export::ExportReceipt> {
        self.apply_metadata_export_controlled(operation, cancel, None)
    }
    pub(crate) fn apply_metadata_export_with_receipt(
        &mut self,
        operation: &str,
        cancel: &std::sync::atomic::AtomicBool,
        attempt: &str,
        request_digest: &str,
        kind: &str,
    ) -> Result<crate::metadata_export::ExportReceipt> {
        self.apply_metadata_export_controlled(
            operation,
            cancel,
            Some((attempt, request_digest, kind)),
        )
    }
    fn apply_metadata_export_controlled(
        &mut self,
        operation: &str,
        cancel: &std::sync::atomic::AtomicBool,
        durable: Option<(&str, &str, &str)>,
    ) -> Result<crate::metadata_export::ExportReceipt> {
        self.require_jobs_released()?;
        self.reconcile_export_paths(512)?;
        // IMMEDIATE prevents a concurrent catalog writer from changing metadata between the
        // revision check and external publication. Filesystem recovery evidence remains durable
        // even if the catalog transaction itself fails after publication.
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some((attempt, digest, _)) = durable {
            ensure!(
                crate::catalog_metadata_write::existing(&tx, attempt, digest)?.is_none(),
                "metadata attempt already committed"
            );
        }
        let stored = crate::catalog_metadata_write::export_plan_row(&tx, operation)?;
        let asset = stored.asset_id;
        let expected = stored.revision;
        ensure!(
            crate::catalog_image_exports::current(&tx, operation, &asset, expected)?,
            "catalog metadata changed since export plan; prepare a new plan"
        );
        let plan: crate::metadata_export::ExportPlan = serde_json::from_str(&stored.plan_json)?;
        let payload = read_blob(&tx, &stored.payload_hash)?;
        if let Some(limits) = plan.alias_limits {
            let mut control = crate::catalog_exports::ExportControl::new(cancel);
            crate::catalog_exports::protect_catalog_original_destination_controlled(
                &tx,
                &self.session,
                &plan.destination,
                limits,
                &mut control,
            )?;
        }
        let receipt = match self.session.apply_metadata_file(&plan, &payload, cancel)? {
            Some(receipt) => receipt,
            None => crate::metadata_export::apply_export(&plan, &payload)?,
        };
        crate::metadata_export::validate_metadata_export_receipt_wire(&receipt, &plan)?;
        tx.execute(
            "UPDATE metadata_export_plans SET receipt=?1 WHERE operation=?2",
            params![serde_json::to_string(&receipt)?, operation],
        )?;
        if let Some((attempt, digest, kind)) = durable {
            let owner = crate::catalog_image_exports::owner(&tx, operation, &asset, expected)?;
            crate::catalog_metadata_write::insert(
                &tx,
                attempt,
                digest,
                kind,
                &owner,
                &serde_json::json!({"operation": operation, "receipt": receipt}),
            )?;
        }
        tx.commit()?;
        drop(_write);
        Ok(receipt)
    }
    pub fn metadata_decisions(
        &self,
        asset: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        revision(&self.db, asset)?;
        ensure!(
            after >= 0 && (1..=100).contains(&limit),
            "decision history requires cursor>=0 and limit1..=100"
        );
        let mut out = Vec::new();
        for row in self.db.prepare("SELECT id,revision,action,detail,created_at FROM metadata_history WHERE asset_id=?1 AND id>?2 ORDER BY id LIMIT ?3")?.query_map(params![asset,after,limit as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))? {
            let (id,revision,action,detail,created_at)=row?;out.push(serde_json::json!({"id":id,"revision":revision,"action":action,"detail":serde_json::from_str::<serde_json::Value>(&detail)?,"created_at":created_at}));
        }
        Ok(out)
    }
}

impl Catalog {
    pub fn recover_metadata_export(
        &mut self,
        directory: &Path,
    ) -> Result<crate::metadata_export::ExportReceipt> {
        self.recover_metadata_export_controlled(
            directory,
            false,
            &std::sync::atomic::AtomicBool::new(false),
            None,
        )
    }
    pub(crate) fn recover_metadata_export_with_receipt(
        &mut self,
        directory: &Path,
        restore_only: bool,
        cancel: &std::sync::atomic::AtomicBool,
        attempt: &str,
        request_digest: &str,
    ) -> Result<crate::metadata_export::ExportReceipt> {
        self.recover_metadata_export_controlled(
            directory,
            restore_only,
            cancel,
            Some((attempt, request_digest)),
        )
    }
    fn recover_metadata_export_controlled(
        &mut self,
        directory: &Path,
        restore_only: bool,
        cancel: &std::sync::atomic::AtomicBool,
        durable: Option<(&str, &str)>,
    ) -> Result<crate::metadata_export::ExportReceipt> {
        self.require_jobs_released()?;
        self.reconcile_export_paths(512)?;
        let name = directory
            .file_name()
            .and_then(|s| s.to_str())
            .context("invalid recovery operation directory")?;
        let operation = name
            .strip_prefix(".photocatalog-xmp-export-")
            .context("not a LensWorks export operation")?;
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some((attempt, digest)) = durable {
            ensure!(
                crate::catalog_metadata_write::existing(&tx, attempt, digest)?.is_none(),
                "metadata attempt already committed"
            );
        }
        let stored = crate::catalog_metadata_write::export_plan_row(&tx, operation)?;
        let asset = stored.asset_id;
        let expected = stored.revision;
        let plan: crate::metadata_export::ExportPlan = serde_json::from_str(&stored.plan_json)?;
        let expected_directory = plan
            .destination
            .parent()
            .context("export destination has no parent")?
            .join(format!(".photocatalog-xmp-export-{}", plan.operation));
        ensure!(
            crate::storage_volume::NativePath::from_path(&expected_directory)
                == crate::storage_volume::NativePath::from_path(directory),
            "recovery operation path differs from catalog plan"
        );
        let current = crate::catalog_image_exports::current(&tx, operation, &asset, expected)?;
        let restore = restore_only || !current;
        if !restore && let Some(limits) = plan.alias_limits {
            let mut control = crate::catalog_exports::ExportControl::new(cancel);
            crate::catalog_exports::protect_catalog_original_destination_controlled(
                &tx,
                &self.session,
                &plan.destination,
                limits,
                &mut control,
            )?;
        }
        let receipt = match if restore {
            self.session.restore_metadata_file(&plan, cancel)?
        } else {
            self.session.recover_metadata_file(&plan, cancel)?
        } {
            Some(receipt) => receipt,
            None if restore => crate::metadata_export::restore_planned_export(&plan)?,
            None => crate::metadata_export::recover_export(&expected_directory)?,
        };
        crate::metadata_export::validate_metadata_export_receipt_wire(&receipt, &plan)?;
        tx.execute(
            "UPDATE metadata_export_plans SET receipt=?1 WHERE operation=?2",
            params![serde_json::to_string(&receipt)?, operation],
        )?;
        if let Some((attempt, digest)) = durable {
            crate::catalog_metadata_write::insert(
                &tx,
                attempt,
                digest,
                if restore_only {
                    "sidecar_restore"
                } else {
                    "sidecar_recover"
                },
                &crate::catalog_image_exports::owner(&tx, operation, &asset, expected)?,
                &serde_json::json!({"operation": operation, "receipt": receipt}),
            )?;
        }
        tx.commit()?;
        Ok(receipt)
    }
}

#[cfg(test)]
mod image_source_state_tests {
    use super::*;
    #[test]
    fn unavailable_shared_source_advances_master_and_follower_once() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        catalog.db.execute("INSERT INTO assets(id,location,path_display,state) VALUES('source',?1,'source','pending')",[b"source".as_slice()])?;
        let path = temp.path().join("source.xmp");
        std::fs::write(&path,br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="3"/></rdf:RDF>"#)?;
        let inspection = xmp_packets::inspect_sidecar(&path, &xmp_packets::Limits::default())?;
        let source = Source {
            kind: "sidecar".into(),
            locator: location_bytes(&path),
            display: "fixture sidecar".into(),
            ambiguous: false,
            provenance: serde_json::json!({"fixture":true}),
        };
        catalog.retain_metadata("source", &source, &inspection)?;
        let master = crate::catalog_edits::VariantKey::master("source");
        let copy = catalog.create_edit_variant(&master, 0, "copy")?.key;
        let before = catalog.metadata_for_image(&master)?.revision;
        let copy_before = catalog.metadata_for_image(&copy)?.revision;
        assert!(catalog.unavailable_metadata_source("source", &source, "missing fixture")?);
        assert_eq!(catalog.metadata_for_image(&master)?.revision, before + 1);
        assert_eq!(catalog.metadata_for_image(&copy)?.revision, copy_before + 1);
        assert_eq!(
            catalog.metadata_for_image(&copy)?.sources[0].availability,
            "missing fixture"
        );
        assert!(!catalog.unavailable_metadata_source("source", &source, "missing fixture")?);
        assert_eq!(catalog.metadata_for_image(&master)?.revision, before + 1);
        assert_eq!(catalog.metadata_for_image(&copy)?.revision, copy_before + 1);
        Ok(())
    }
}

#[cfg(test)]
mod managed_discovery_tests {
    use super::*;
    #[test]
    fn bounded_directory_facts_preserve_native_names_and_rollback_incomplete_roster() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let directory = temp.path().canonicalize()?;
        let discovery = ImportDiscovery::new()?;
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let original = directory.join("photo.cr2");
        let sidecar = directory.join("photo.cr2.xmp");
        let paths = vec![original.clone(), sidecar.clone()];
        #[cfg(unix)]
        let paths = {
            use std::os::unix::ffi::OsStringExt;
            let mut paths = paths;
            paths.push(directory.join(std::ffi::OsString::from_vec(b"native\xff.xmp".to_vec())));
            paths
        };
        index_directory_facts(
            &discovery.db,
            &directory,
            paths.iter().cloned().map(|path| {
                Ok(DirectoryFact {
                    path,
                    regular: true,
                })
            }),
            Some(&cancel),
        )?;
        for path in &paths {
            let stored: Vec<u8> = discovery.db.query_row(
                "SELECT path FROM metadata_scan_files WHERE directory=?1 AND name=?2",
                params![
                    location_bytes(&directory),
                    location_bytes(Path::new(path.file_name().unwrap()))
                ],
                |r| r.get(0),
            )?;
            assert_eq!(stored, location_bytes(path));
        }
        assert_eq!(discovery.sidecars(&original, &cancel)?.len(), 1);
        initialize_discovery(&discovery.db)?;
        let error = index_directory_facts(
            &discovery.db,
            &directory,
            [
                Ok(DirectoryFact {
                    path: original.clone(),
                    regular: true,
                }),
                Ok(DirectoryFact {
                    path: directory.join("wrong/elsewhere.xmp"),
                    regular: true,
                }),
            ],
            Some(&cancel),
        )
        .unwrap_err();
        assert!(error.to_string().contains("another parent"));
        assert_eq!(
            discovery
                .db
                .query_row("SELECT count(*) FROM metadata_scan_files", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        assert_eq!(
            discovery
                .db
                .query_row("SELECT count(*) FROM metadata_scan_dirs", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        let too_many = (0..=xmp_packets::Limits::default().max_entries).map(|_| {
            Ok(DirectoryFact {
                path: original.clone(),
                regular: false,
            })
        });
        assert!(
            index_directory_facts(&discovery.db, &directory, too_many, Some(&cancel))
                .unwrap_err()
                .to_string()
                .contains("entry limit")
        );
        assert_eq!(
            discovery
                .db
                .query_row("SELECT count(*) FROM metadata_scan_dirs", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        Ok(())
    }
}
