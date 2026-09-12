//! File-owned XMP reconstructed exclusively from completed selected custody.
//! No source filesystem access occurs here. Historical and supplemental evidence
//! remain separate observations; inspection completeness is never invented.
use super::{
    evidence,
    organization::{Evidence, SourceRecord},
    retention,
};
use crate::{
    Catalog,
    catalog_metadata::{self, Prepared, Source},
    catalog_writer::Priority,
    lightroom::migration_source::{Collection, EvidenceRecord, Field, InputSeal, MigrationSource},
    lightroom::plan::Cell,
    storage_volume::NativePath,
    xmp_packets::{self, Inspection, Issue, Packet, ParseInput, SourceRevision, Status},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const META: usize = 8 * 1024 * 1024;
const PAYLOAD: usize = 64 * 1024 * 1024;
const PACKET: usize = 16 * 1024 * 1024;
const COUNT: usize = 1024;
const ADAPTER: &str = "lightroom-file-metadata-v1";
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    Embedded,
    SidecarXmp,
    SidecarUpper,
    AppendedXmp,
    AppendedUpper,
}
impl Origin {
    fn name(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::SidecarXmp => "sidecar_xmp",
            Self::SidecarUpper => "sidecar_XMP",
            Self::AppendedXmp => "sidecar_appended_xmp",
            Self::AppendedUpper => "sidecar_appended_XMP",
        }
    }
    fn kind(self) -> &'static str {
        if self == Self::Embedded {
            "embedded"
        } else {
            "sidecar"
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Association {
    /// Inspection of a candidate name does not establish its unique owner.
    Unresolved,
    /// Explicit source-bound coordinator decision, never inferred from Complete.
    Confirmed { reason: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Projection {
    pub file: SourceRecord,
    pub retained_path: i64,
    pub origin: Origin,
    /// Exact destination records for this origin, independently enumerated in
    /// the sealed source. Caller-selected subsets cannot claim completeness.
    pub packet_records: Vec<i64>,
    pub import_source: String,
    pub association: Association,
    /// Exact retained proof document whose digest is in the source InputSeal.
    pub supplement: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionResult {
    pub input_digest: String,
    pub asset_id: String,
    pub state: String,
    pub historical_status: Option<Status>,
    pub observation: Option<i64>,
    pub reason: Option<String>,
}
/// Separately retained proof bytes, bound by SupplementPin.proof_blake3.
/// Payload evidence IDs are locators, never authority without their expected hash.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplementalProof {
    pub protocol: u32,
    pub revision: String,
    pub source_id: String,
    pub origin: String,
    pub source_revision: SourceRevision,
    pub historical_status: Status,
    pub status: Status,
    pub issues: Vec<Issue>,
    /// Exact original validation document retained alongside this normalized proof.
    pub validation_document: Payload,
    pub packets: Vec<SupplementPacket>,
    pub parse_inputs: Vec<SupplementInput>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payload {
    pub evidence: String,
    pub length: u64,
    pub blake3: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplementPacket {
    pub payload: Payload,
    pub container: xmp_packets::Container,
    pub ranges: Vec<xmp_packets::ByteRange>,
    pub group: String,
    pub attributes: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplementInput {
    pub payload: Payload,
    pub transformation: xmp_packets::Transformation,
    pub packet_indices: Vec<usize>,
    pub group: String,
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS migration_file_metadata(
        file_source TEXT NOT NULL, origin TEXT NOT NULL, supplement TEXT NOT NULL,
        owner TEXT NOT NULL, input_digest TEXT NOT NULL, retained_file INTEGER NOT NULL REFERENCES migration_retained_records(sequence),
        retained_path INTEGER NOT NULL REFERENCES migration_retained_records(sequence),
        proof BLOB NOT NULL, result BLOB NOT NULL,
        PRIMARY KEY(file_source,origin,supplement));")?;
    Ok(())
}
fn encoded<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    crate::lightroom::bounded_json(v, META)
}
fn text(db: &Connection, seq: i64, r: &EvidenceRecord, n: &str, cap: usize) -> Result<String> {
    Ok(String::from_utf8(retention::field_bytes(
        db, seq, r, n, cap,
    )?)?)
}
fn field_length(r: &EvidenceRecord, n: &str) -> Result<u64> {
    match r.fields.get(n).context("missing payload field")? {
        Field::Bytes(v) => Ok(v.bytes),
        Field::Inline(Cell::Text(v) | Cell::Blob(v)) => Ok(v.len() as u64),
        Field::Inline(Cell::Null) => Ok(0),
        _ => anyhow::bail!("payload is not bytes"),
    }
}
fn mapped(db: &Connection, request: &Projection) -> Result<String> {
    let (asset, owner): (String, String) = db
        .query_row(
            "SELECT asset_id,import_source FROM migration_originals WHERE source_identity=?",
            [request.file.source.identity()?],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .context("file has no explicit original mapping")?;
    ensure!(
        owner == request.import_source,
        "file mapping belongs to another importer"
    );
    Ok(asset)
}
fn input(db: &Connection, record: i64) -> Result<(String, InputSeal)> {
    let (id,raw):(String,Vec<u8>)=db.query_row("SELECT i.id,i.seal FROM migration_retention i JOIN migration_retained_records r ON r.input=i.id WHERE r.sequence=? AND r.complete=1",[record],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(raw.len() <= META, "retained seal size");
    let seal: InputSeal = serde_json::from_slice(&raw)?;
    ensure!(seal.binding_blake3()? == id, "retained seal changed");
    Ok((id, seal))
}
fn previous(
    db: &Connection,
    request: &Projection,
    digest: &str,
    supplement: &str,
) -> Result<Option<ProjectionResult>> {
    let value:Option<(String,String,Vec<u8>)>=db.query_row("SELECT owner,input_digest,result FROM migration_file_metadata WHERE file_source=?1 AND origin=?2 AND supplement=?3",params![request.file.source.identity()?,request.origin.name(),supplement],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    value
        .map(|(owner, old, raw)| {
            ensure!(
                owner == request.import_source && old == digest,
                "file metadata decision changed; explicit reconciliation required"
            );
            ensure!(raw.len() <= META, "file metadata result limit");
            Ok(serde_json::from_slice(&raw)?)
        })
        .transpose()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Observation {
    origin: String,
    #[serde(default)]
    status: Option<Status>,
    #[serde(default)]
    revision: Option<SourceRevision>,
    #[serde(default)]
    packets: Option<usize>,
    #[serde(default)]
    parse_inputs: Option<usize>,
    #[serde(default)]
    issues: Vec<Issue>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}
#[derive(Deserialize)]
struct PacketDetail {
    container: xmp_packets::Container,
    ranges: Vec<xmp_packets::ByteRange>,
    group: String,
    attributes: BTreeMap<String, String>,
    source_revision: SourceRevision,
    inspection_status: Status,
    source_path: NativePath,
}
#[derive(Deserialize)]
struct ParseDetail {
    transformation: xmp_packets::Transformation,
    packet_indices: Vec<usize>,
    group: String,
    input_blake3: String,
    source_revision: SourceRevision,
    inspection_status: Status,
}
fn origin_path(base: &NativePath, origin: Origin) -> Result<NativePath> {
    if origin == Origin::Embedded {
        return Ok(base.clone());
    }
    // Pure host-native path operations; foreign encodings remain retained-only.
    let path = base.to_path()?;
    let suffix = if matches!(origin, Origin::SidecarUpper | Origin::AppendedUpper) {
        "XMP"
    } else {
        "xmp"
    };
    Ok(NativePath::from_path(
        &if matches!(origin, Origin::AppendedXmp | Origin::AppendedUpper) {
            let mut name = path.into_os_string();
            name.push(format!(".{suffix}"));
            std::path::PathBuf::from(name)
        } else {
            path.with_extension(suffix)
        },
    ))
}
fn indices(values: &[usize], packets: usize) -> Result<()> {
    ensure!(
        !values.is_empty() && values.len() <= COUNT,
        "parse input packet-reference count"
    );
    ensure!(
        values.iter().all(|i| *i < packets),
        "parse input references missing packet"
    );
    Ok(())
}
fn ranges(values: &[xmp_packets::ByteRange], revision: &SourceRevision) -> Result<()> {
    ensure!(values.len() <= 100_000, "packet range count");
    for v in values {
        ensure!(
            v.offset <= revision.length && v.length <= revision.length - v.offset,
            "packet range exceeds original source"
        );
    }
    Ok(())
}

/// Lightweight guards allow a descriptor-heavy origin to receive a retained-only
/// result without decoding more than the interpretation budget. Collection 7 is
/// the fixed Packets slot in the destination retention schema installed with this module.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PacketGuard {
    sequence: i64,
    input: String,
    revision: String,
    collection: i64,
    rowid: i64,
    digest: String,
    length: usize,
    complete: bool,
}
fn packet_guard(db: &Connection, sequence: i64) -> Result<PacketGuard> {
    Ok(db.query_row("SELECT input,revision,collection,source_rowid,digest,raw_length,complete FROM migration_retained_records WHERE sequence=?",[sequence],|r|Ok(PacketGuard{sequence,input:r.get(0)?,revision:r.get(1)?,collection:r.get(2)?,rowid:r.get(3)?,digest:r.get(4)?,length:evidence::size(r,5)?,complete:r.get(6)?}))?)
}
fn packet_guards(
    db: &Connection,
    request: &Projection,
    binding: &str,
) -> Result<(Vec<PacketGuard>, bool)> {
    let mut total = 0usize;
    for sequence in [request.file.retained_record, request.retained_path] {
        let length: usize = db.query_row(
            "SELECT raw_length FROM migration_retained_records WHERE sequence=?",
            [sequence],
            |r| evidence::size(r, 0),
        )?;
        total = total
            .checked_add(length)
            .context("descriptor byte overflow")?;
    }
    let mut result = Vec::new();
    for sequence in &request.packet_records {
        let guard = packet_guard(db, *sequence)?;
        ensure!(
            guard.input == binding
                && guard.revision == request.file.source.capture_revision
                && guard.collection == 7
                && guard.complete,
            "packet custody scope differs"
        );
        total = total
            .checked_add(guard.length)
            .context("descriptor byte overflow")?;
        result.push(guard);
    }
    Ok((result, total <= META))
}

struct Historical {
    observation: Observation,
    records: Vec<(i64, EvidenceRecord)>,
    packet_slots: BTreeMap<usize, usize>,
    input_slots: BTreeMap<usize, usize>,
    path: NativePath,
}
fn historical(
    db: &Connection,
    proof: &mut Evidence,
    request: &Projection,
    source_id: &str,
    path: &EvidenceRecord,
    descriptors_fit: bool,
) -> Result<Historical> {
    let body = retention::field_bytes(db, request.retained_path, path, "evidence", META)?;
    let value: serde_json::Value = serde_json::from_slice(&body)?;
    let observations = value.get("inspections").and_then(|v| v.as_array());
    let observation = if let Some(values) = observations {
        ensure!(values.len() <= 5, "unexpected origin roster");
        let mut names = BTreeSet::new();
        for v in values {
            ensure!(
                names.insert(
                    v.get("origin")
                        .and_then(|x| x.as_str())
                        .context("inspection origin missing")?
                ),
                "duplicate inspection origin"
            );
        }
        let chosen = values
            .iter()
            .filter(|v| v.get("origin").and_then(|x| x.as_str()) == Some(request.origin.name()))
            .collect::<Vec<_>>();
        ensure!(chosen.len() == 1, "origin observation missing or ambiguous");
        serde_json::from_value::<Observation>(chosen[0].clone())?
    } else {
        Observation {
            origin: request.origin.name().into(),
            status: None,
            revision: None,
            packets: None,
            parse_inputs: None,
            issues: vec![],
            state: None,
            error: Some("No retained inspection for this path".into()),
        }
    };
    let base: NativePath = serde_json::from_str(&text(
        db,
        request.retained_path,
        path,
        "inspection_path",
        META,
    )?)?;
    let mut result = Historical {
        observation,
        records: vec![],
        packet_slots: BTreeMap::new(),
        input_slots: BTreeMap::new(),
        path: base,
    };
    for sequence in request.packet_records.iter().filter(|_| descriptors_fit) {
        let record = proof.record(db, *sequence)?;
        proof.same_input(request.file.retained_record, *sequence)?;
        ensure!(
            record.collection == Collection::Packets
                && record.revision == request.file.source.capture_revision,
            "packet is not from selected file capture"
        );
        ensure!(
            text(db, *sequence, &record, "source_id", 4096)? == source_id,
            "packet belongs to another file"
        );
        let origin = text(db, *sequence, &record, "origin", 128)?;
        let suffix = origin
            .strip_prefix(&format!("{}:", request.origin.name()))
            .context("packet origin differs")?;
        let (kind, index) = suffix.split_once(':').context("packet index missing")?;
        let index: usize = index.parse()?;
        ensure!(
            index < COUNT && origin == format!("{}:{kind}:{index}", request.origin.name()),
            "packet index is noncanonical or exceeds limit"
        );
        let slots = match kind {
            "packet" => &mut result.packet_slots,
            "parse_input" => &mut result.input_slots,
            _ => anyhow::bail!("unknown origin packet row"),
        };
        ensure!(
            slots.insert(index, result.records.len()).is_none(),
            "duplicate packet index"
        );
        result.records.push((*sequence, record));
    }
    let packets = result.observation.packets.unwrap_or(0);
    let inputs = result.observation.parse_inputs.unwrap_or(0);
    ensure!(
        packets <= COUNT && inputs <= COUNT,
        "origin count exceeds native supported limit"
    );
    ensure!(
        if descriptors_fit {
            result.packet_slots.keys().copied().eq(0..packets)
                && result.input_slots.keys().copied().eq(0..inputs)
        } else {
            request.packet_records.len() == packets + inputs
        },
        "origin packet/parse-input count or contiguous roster differs"
    );
    if result.observation.status.is_some() {
        ensure!(
            result.observation.revision.is_some()
                && result.observation.packets.is_some()
                && result.observation.parse_inputs.is_some(),
            "inspection proof incomplete"
        );
    }
    Ok(result)
}
fn reconstruct(db: &Connection, h: &Historical, origin: Origin) -> Result<Option<Inspection>> {
    let Some(revision) = &h.observation.revision else {
        return Ok(None);
    };
    let Some(status) = h.observation.status else {
        return Ok(None);
    };
    if status != Status::Complete {
        return Ok(None);
    }
    let Ok(source_path) = origin_path(&h.path, origin) else {
        return Ok(None);
    };
    let mut raw_total = 0u64;
    let mut decoded_total = 0u64;
    let mut metadata_total = 0u64;
    for (_, record) in &h.records {
        metadata_total = metadata_total
            .checked_add(field_length(record, "detail")?)
            .context("packet metadata overflow")?;
    }
    if metadata_total > META as u64 {
        return Ok(None);
    }
    for slot in h.packet_slots.values() {
        let n = field_length(&h.records[*slot].1, "raw")?;
        if n > PACKET as u64 {
            return Ok(None);
        }
        raw_total = raw_total.checked_add(n).context("packet byte overflow")?;
    }
    for slot in h.input_slots.values() {
        let n = field_length(&h.records[*slot].1, "decoded")?;
        if n > PACKET as u64 {
            return Ok(None);
        }
        decoded_total = decoded_total
            .checked_add(n)
            .context("parse byte overflow")?;
    }
    if raw_total > PAYLOAD as u64 || decoded_total > PAYLOAD as u64 {
        return Ok(None);
    }
    let mut out = Inspection {
        revision: revision.clone(),
        status,
        issues: h.observation.issues.clone(),
        packets: vec![],
        parse_inputs: vec![],
    };
    for slot in h.packet_slots.values() {
        let (sequence, r) = &h.records[*slot];
        let detail: serde_json::Value =
            serde_json::from_str(&text(db, *sequence, r, "detail", META)?)?;
        if serde_json::from_value::<xmp_packets::Container>(
            detail
                .get("container")
                .context("packet container missing")?
                .clone(),
        )
        .is_err()
        {
            return Ok(None);
        }
        let d: PacketDetail = serde_json::from_value(detail)?;
        ensure!(
            d.source_revision == *revision
                && d.inspection_status == status
                && d.source_path == source_path,
            "packet source revision/status/path differs"
        );
        ranges(&d.ranges, revision)?;
        let raw = retention::field_bytes(db, *sequence, r, "raw", PACKET)?;
        let hash = text(db, *sequence, r, "raw_digest", 64)?;
        ensure!(
            blake3::hash(&raw).to_hex().as_str() == hash,
            "packet raw hash differs"
        );
        ensure!(
            field_length(r, "decoded")? == 0,
            "raw carrier unexpectedly includes parse payload"
        );
        out.packets.push(Packet {
            container: d.container,
            bytes: raw,
            blake3: hash,
            ranges: d.ranges,
            group: d.group,
            attributes: d.attributes,
        });
    }
    for slot in h.input_slots.values() {
        let (sequence, r) = &h.records[*slot];
        let detail: serde_json::Value =
            serde_json::from_str(&text(db, *sequence, r, "detail", META)?)?;
        if serde_json::from_value::<xmp_packets::Transformation>(
            detail
                .get("transformation")
                .context("parse transformation missing")?
                .clone(),
        )
        .is_err()
        {
            return Ok(None);
        }
        let d: ParseDetail = serde_json::from_value(detail)?;
        ensure!(
            d.source_revision == *revision && d.inspection_status == status,
            "parse source revision/status differs"
        );
        indices(&d.packet_indices, out.packets.len())?;
        ensure!(
            field_length(r, "raw")? == 0,
            "parse row raw must remain empty"
        );
        let bytes = retention::field_bytes(db, *sequence, r, "decoded", PACKET)?;
        let hash = text(db, *sequence, r, "raw_digest", 64)?;
        ensure!(
            blake3::hash(&bytes).to_hex().as_str() == hash && hash == d.input_blake3,
            "parse input hash differs"
        );
        out.parse_inputs.push(ParseInput {
            bytes,
            blake3: hash,
            packet_indices: d.packet_indices,
            transformation: d.transformation,
            group: d.group,
        });
    }
    Ok(Some(out))
}
fn custody(catalog: &Catalog, id: &str, maximum: usize) -> Result<Vec<u8>> {
    ensure!(
        id.len() == 64 && id.bytes().all(|c| c.is_ascii_hexdigit()),
        "evidence ID bounds"
    );
    let state = catalog.migration_evidence(id)?;
    ensure!(
        state.complete && state.length <= maximum as u64,
        "complete bounded supplemental custody required"
    );
    let mut bytes = Vec::new();
    while (bytes.len() as u64) < state.length {
        let part = evidence::read(&catalog.db, id, bytes.len() as u64)?;
        ensure!(
            !part.is_empty() && part.len() <= maximum - bytes.len(),
            "supplement chunk length"
        );
        bytes.extend(part);
    }
    ensure!(
        bytes.len() as u64 == state.length,
        "supplement custody length differs"
    );
    Ok(bytes)
}
fn payload(catalog: &Catalog, p: &Payload, maximum: usize) -> Result<Vec<u8>> {
    let bytes = custody(catalog, &p.evidence, maximum)?;
    ensure!(
        bytes.len() as u64 == p.length && blake3::hash(&bytes).to_hex().as_str() == p.blake3,
        "supplement payload hash/length differs"
    );
    Ok(bytes)
}
fn supplement(
    catalog: &Catalog,
    request: &Projection,
    seal: &InputSeal,
    source_id: &str,
    h: &Historical,
) -> Result<Option<(String, String, Option<Inspection>)>> {
    let Some(id) = &request.supplement else {
        return Ok(None);
    };
    ensure!(
        request.origin == Origin::Embedded,
        "only exact embedded supplement supported"
    );
    let bytes = custody(catalog, id, META)?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let pins = seal
        .supplements
        .iter()
        .filter(|p| {
            p.revision == request.file.source.capture_revision
                && p.source_id == source_id
                && p.origin == request.origin.name()
                && p.proof_blake3 == hash
        })
        .collect::<Vec<_>>();
    ensure!(
        pins.len() == 1,
        "supplement proof is not exactly pinned by selected seal"
    );
    let pin = pins[0];
    let value: SupplementalProof = serde_json::from_slice(&bytes)?;
    ensure!(
        value.protocol == 1
            && value.revision == pin.revision
            && value.source_id == pin.source_id
            && value.origin == pin.origin
            && value.source_revision == pin.source_revision
            && value.historical_status == pin.historical_status,
        "supplement proof association differs"
    );
    ensure!(
        h.observation.status == Some(pin.historical_status)
            && h.observation.revision.as_ref() == Some(&pin.source_revision),
        "supplement historical source status/revision differs"
    );
    ensure!(
        value.packets.len() <= COUNT && value.parse_inputs.len() <= COUNT,
        "supplement origin count exceeds native support"
    );
    payload(catalog, &value.validation_document, META)?;
    // The semantic identity does not contain transport IDs or inspection lineage.
    let mut semantic = serde_json::to_value(&value)?;
    semantic.as_object_mut().unwrap().remove("source_id");
    semantic["validation_document"]
        .as_object_mut()
        .unwrap()
        .remove("evidence");
    for key in ["packets", "parse_inputs"] {
        for item in semantic[key].as_array_mut().unwrap() {
            item["payload"].as_object_mut().unwrap().remove("evidence");
        }
    }
    let semantic_hash = blake3::hash(&encoded(&semantic)?).to_hex().to_string();
    let raw = value.packets.iter().try_fold(0u64, |s, p| {
        s.checked_add(p.payload.length)
            .context("supplement raw overflow")
    })?;
    let decoded = value.parse_inputs.iter().try_fold(0u64, |s, p| {
        s.checked_add(p.payload.length)
            .context("supplement parse overflow")
    })?;
    if value.status != Status::Complete
        || raw > PAYLOAD as u64
        || decoded > PAYLOAD as u64
        || value
            .packets
            .iter()
            .any(|p| p.payload.length > PACKET as u64)
        || value
            .parse_inputs
            .iter()
            .any(|p| p.payload.length > PACKET as u64)
    {
        return Ok(Some((hash, semantic_hash, None)));
    }
    let mut inspection = Inspection {
        revision: value.source_revision,
        status: value.status,
        issues: value.issues,
        packets: vec![],
        parse_inputs: vec![],
    };
    for p in value.packets {
        ranges(&p.ranges, &inspection.revision)?;
        let bytes = payload(catalog, &p.payload, PACKET)?;
        inspection.packets.push(Packet {
            bytes,
            blake3: p.payload.blake3,
            container: p.container,
            ranges: p.ranges,
            group: p.group,
            attributes: p.attributes,
        });
    }
    for p in value.parse_inputs {
        indices(&p.packet_indices, inspection.packets.len())?;
        let bytes = payload(catalog, &p.payload, PACKET)?;
        inspection.parse_inputs.push(ParseInput {
            bytes,
            blake3: p.payload.blake3,
            packet_indices: p.packet_indices,
            transformation: p.transformation,
            group: p.group,
        });
    }
    Ok(Some((hash, semantic_hash, Some(inspection))))
}
impl Catalog {
    /// One complete selected file/origin. Reading and preparation precede writer
    /// admission; only original mapping, native observation and checkpoint commit.
    pub fn project_migration_file_metadata(
        &mut self,
        source: Option<&MigrationSource>,
        request: &Projection,
    ) -> Result<ProjectionResult> {
        request.file.source.identity()?;
        ensure!(
            request.file.source.table == "AgLibraryFile",
            "file metadata requires AgLibraryFile"
        );
        ensure!(
            !request.import_source.is_empty()
                && request.import_source.len() <= 4096
                && !request.import_source.contains('\0'),
            "import owner bounds"
        );
        ensure!(
            request.packet_records.len() <= 2 * COUNT,
            "packet proof roster limit"
        );
        ensure!(
            request.packet_records.iter().all(|v| *v > 0)
                && request
                    .packet_records
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .len()
                    == request.packet_records.len(),
            "packet proof repeats a record"
        );
        if let Association::Confirmed { reason } = &request.association {
            ensure!(
                !reason.trim().is_empty() && reason.len() <= 16384,
                "association decision requires bounded reason"
            );
        }
        let mut proof = Evidence::with_record_limit(2052)?;
        let source_id = proof.source(&self.db, &request.file)?;
        let path = proof.record(&self.db, request.retained_path)?;
        proof.same_input(request.file.retained_record, request.retained_path)?;
        ensure!(
            path.collection == Collection::Paths
                && path.revision == request.file.source.capture_revision
                && text(&self.db, request.retained_path, &path, "source_id", 4096)? == source_id,
            "path does not belong to selected file"
        );
        let (binding, seal) = input(&self.db, request.file.retained_record)?;
        let (packet_guards, descriptors_fit) = packet_guards(&self.db, request, &binding)?;
        let historical = historical(
            &self.db,
            &mut proof,
            request,
            &source_id,
            &path,
            descriptors_fit,
        )?;
        let mut supplemental = supplement(self, request, &seal, &source_id, &historical)?;
        let supplement_semantic = supplemental
            .as_ref()
            .map(|s| s.1.clone())
            .unwrap_or_default();
        let digest=blake3::hash(&encoded(&serde_json::json!({"adapter":ADAPTER,"source":request.file.source,"owner":request.import_source,"origin":request.origin,"observation":historical.observation,"association":request.association,"supplement":supplement_semantic}))?).to_hex().to_string();
        if let Some(old) = previous(&self.db, request, &digest, &supplement_semantic)? {
            return Ok(old);
        }
        let source =
            source.context("first origin projection requires sealed source enumeration")?;
        ensure!(
            source.binding_blake3() == binding,
            "enumeration belongs to another selected seal"
        );
        let mut rowids = packet_guards.iter().map(|r| r.rowid).collect::<Vec<_>>();
        rowids.sort_unstable();
        ensure!(
            source.origin_packet_roster(
                &request.file.source.capture_revision,
                &source_id,
                request.origin.name()
            )? == rowids,
            "origin packet enumeration differs; omitted or extra rows"
        );
        let asset = mapped(&self.db, request)?;
        let inspection = if let Some((_, _, inspection)) = &mut supplemental {
            inspection.take()
        } else if descriptors_fit {
            reconstruct(&self.db, &historical, request.origin)?
        } else {
            None
        };
        let ambiguous = request.origin != Origin::Embedded
            && matches!(request.association, Association::Unresolved);
        let prepared=inspection.as_ref().map(|inspection| {
            let location=origin_path(&historical.path,request.origin)?;
            let source=Source{kind:request.origin.kind().into(),locator:format!("lightroom:{}:{}:{}",request.file.source.identity()?,request.origin.name(),supplement_semantic).into_bytes(),display:format!("Lightroom retained {}",request.origin.name()),ambiguous,provenance:serde_json::json!({"adapter":ADAPTER,"file":request.file.source,"origin":request.origin,"source_path":location,"historical_observation":historical.observation,"association":request.association,"supplement_proof_blake3":supplemental.as_ref().map(|s|&s.0)})};
            Ok::<_,anyhow::Error>((Prepared::new(inspection,&source)?,source))
        }).transpose()?;
        let mut result = ProjectionResult {
            input_digest: digest,
            asset_id: asset.clone(),
            state: if prepared.is_none() {
                "retained_only"
            } else if ambiguous {
                "ambiguous_metadata_retained"
            } else {
                "metadata_retained"
            }
            .into(),
            historical_status: historical.observation.status,
            observation: None,
            reason: if prepared.is_none() {
                Some("Incomplete, unavailable, foreign-path or oversized interpretation; complete original evidence retained without automatic projection".into())
            } else if ambiguous {
                Some("Candidate sidecar ownership remains unresolved; native models retained as ambiguous".into())
            } else {
                None
            },
        };
        let receipts = encoded(
            &serde_json::json!({"file":request.file.retained_record,"path":request.retained_path,"packet_records":request.packet_records,"supplement_evidence":request.supplement,"source_binding":binding}),
        )?;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        proof.recheck(&tx)?;
        for guard in &packet_guards {
            ensure!(
                packet_guard(&tx, guard.sequence)? == *guard,
                "packet custody changed before commit"
            );
        }
        ensure!(
            mapped(&tx, request)? == asset,
            "original file mapping changed"
        );
        if let Some(old) = previous(&tx, request, &result.input_digest, &supplement_semantic)? {
            tx.commit()?;
            return Ok(old);
        }
        if let Some((prepared, source)) = prepared {
            let change = catalog_metadata::retain_prepared(&tx, &asset, &source, &prepared, true)?;
            result.observation = Some(change.observation_id);
        }
        tx.execute(
            "INSERT INTO migration_file_metadata VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                request.file.source.identity()?,
                request.origin.name(),
                supplement_semantic,
                request.import_source,
                result.input_digest,
                request.file.retained_record,
                request.retained_path,
                receipts,
                encoded(&result)?
            ],
        )?;
        tx.commit()?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_edits::VariantKey;
    use crate::catalog_migration::originals::{OriginalDecision, OriginalRequest, SourceKey};
    use crate::lightroom::migration_source::{SupplementPin, tests::Fixture};
    const XML:&[u8]=br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:u="urn:opaque" xmp:Rating="4"><u:unknown>keep all</u:unknown></rdf:Description></rdf:RDF>"#;
    struct Test {
        _fixture: Fixture,
        _temp: tempfile::TempDir,
        source: MigrationSource,
        catalog: Catalog,
        request: Projection,
        asset: String,
    }
    fn upload(c: &mut Catalog, bytes: &[u8], name: &str) -> Result<Payload> {
        let state = c.begin_migration_evidence(name.as_bytes(), bytes.len() as u64)?;
        for (n, part) in bytes.chunks(evidence::CHUNK_BYTES).enumerate() {
            c.append_migration_evidence(&state.id, (n * evidence::CHUNK_BYTES) as u64, part)?;
        }
        Ok(Payload {
            evidence: state.id,
            length: bytes.len() as u64,
            blake3: blake3::hash(bytes).to_hex().to_string(),
        })
    }
    impl Test {
        fn new(origin: Origin, status: Status, supplemental: bool, hidden: bool) -> Result<Self> {
            let mut fixture = Fixture::new();
            let revision = fixture.revision().to_owned();
            let approval = b"explicit selected synthetic file metadata import";
            fixture.seal.approval.document_blake3 = blake3::hash(approval).to_hex().to_string();
            let temp = tempfile::tempdir()?;
            let mut catalog = Catalog::open(temp.path().join("catalog"))?;
            let base = NativePath::from_path(&temp.path().join("never-opened-original.psd"));
            let location = origin_path(&base, origin)?;
            let rev = SourceRevision {
                length: 100000,
                blake3: "a".repeat(64),
                modified_unix_ns: Some(42),
            };
            fixture.edit(|db| {
                db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?1,'file-id','AgLibraryFile',?2,'[]')",params![revision,serde_json::to_string(&vec![Cell::Integer(17)]).unwrap()]).unwrap();
                let observation=serde_json::json!({"origin":origin.name(),"status":status,"revision":rev,"packets":1,"parse_inputs":1,"issues":[]});
                db.execute("INSERT INTO paths(revision,source_id,original,inspection_path,state,evidence) VALUES(?1,'file-id','original',?2,'available_packets_retained',?3)",params![revision,serde_json::to_string(&base).unwrap(),serde_json::json!({"inspections":[observation]}).to_string()]).unwrap();
                let hash=blake3::hash(XML).to_hex().to_string();
                let detail=serde_json::json!({"container":xmp_packets::Container::PsdResource1060,"ranges":[{"offset":123,"length":XML.len()}],"group":"original:packet:0","attributes":{"unknown-container-key":"retain"},"source_revision":rev,"inspection_status":status,"source_path":location});
                db.execute("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,detail) VALUES(?1,'file-id',?2,?3,?4,?5)",params![revision,format!("{}:packet:0",origin.name()),hash,XML,detail.to_string()]).unwrap();
                let detail=serde_json::json!({"transformation":xmp_packets::Transformation::Identity,"packet_indices":[0],"group":"original:packet:0","input_blake3":hash,"source_revision":rev,"inspection_status":status});
                db.execute("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,decoded,detail) VALUES(?1,'file-id',?2,?3,x'',?4,?5)",params![revision,format!("{}:parse_input:0",origin.name()),hash,XML,detail.to_string()]).unwrap();
                if hidden {db.execute("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,detail) VALUES(?1,'file-id',?2,'hidden',x'00','{}')",params![revision,format!("{}:packet:99",origin.name())]).unwrap();}
            });
            let supplement = if supplemental {
                let payload = upload(&mut catalog, XML, "qualified supplemental bytes")?;
                let validation_document = upload(
                    &mut catalog,
                    b"original exact independent validation document",
                    "original validation",
                )?;
                let document = SupplementalProof {
                    protocol: 1,
                    revision: revision.clone(),
                    source_id: "file-id".into(),
                    origin: origin.name().into(),
                    source_revision: rev.clone(),
                    historical_status: status,
                    status: Status::Complete,
                    issues: vec![],
                    validation_document,
                    packets: vec![SupplementPacket {
                        payload: payload.clone(),
                        container: xmp_packets::Container::PsdResource1060,
                        ranges: vec![xmp_packets::ByteRange {
                            offset: 123,
                            length: XML.len() as u64,
                        }],
                        group: "original:packet:0".into(),
                        attributes: BTreeMap::new(),
                    }],
                    parse_inputs: vec![SupplementInput {
                        payload,
                        transformation: xmp_packets::Transformation::Identity,
                        packet_indices: vec![0],
                        group: "original:packet:0".into(),
                    }],
                };
                let p = upload(
                    &mut catalog,
                    &encoded(&document)?,
                    "qualified supplement manifest",
                )?;
                fixture.seal.supplements.push(SupplementPin {
                    revision: revision.clone(),
                    source_id: "file-id".into(),
                    origin: origin.name().into(),
                    source_revision: rev,
                    historical_status: status,
                    proof_blake3: p.blake3,
                });
                Some(p.evidence)
            } else {
                None
            };
            let source = fixture.open();
            catalog.begin_migration_retention(&source, approval)?;
            for _ in 0..1000 {
                if catalog.step_migration_retention(&source)?.complete {
                    break;
                }
            }
            ensure!(
                catalog
                    .migration_retention_progress(source.binding_blake3())?
                    .complete,
                "fixture custody incomplete"
            );
            let rows = catalog.retained_migration_records(
                source.binding_blake3(),
                &revision,
                Collection::Rows,
                0,
                100,
            )?;
            let row = rows
                .into_iter()
                .find(|(_, r)| r.fields["source_id"].text().ok() == Some("file-id"))
                .unwrap();
            let file = SourceRecord {
                retained_record: row.0,
                source: SourceKey {
                    capture_revision: revision.clone(),
                    table: "AgLibraryFile".into(),
                    key: vec![Cell::Integer(17)],
                },
            };
            let path = catalog
                .retained_migration_records(
                    source.binding_blake3(),
                    &revision,
                    Collection::Paths,
                    0,
                    100,
                )?
                .into_iter()
                .find(|(_, r)| r.fields["source_id"].text().ok() == Some("file-id"))
                .unwrap()
                .0;
            let packet_records = catalog
                .retained_migration_records(
                    source.binding_blake3(),
                    &revision,
                    Collection::Packets,
                    0,
                    100,
                )?
                .into_iter()
                .filter(|(_, r)| {
                    r.fields["source_id"].text().ok() == Some("file-id")
                        && !r.fields["origin"].text().unwrap().ends_with(":99")
                })
                .map(|(s, _)| s)
                .collect();
            let asset = catalog
                .register_migration_original(&OriginalRequest {
                    import_source: "lightroom".into(),
                    source: file.source.clone(),
                    retained_record: file.retained_record,
                    decision: OriginalDecision::Create { path: base },
                })?
                .asset_id;
            Ok(Self {
                _fixture: fixture,
                _temp: temp,
                source,
                catalog,
                request: Projection {
                    file,
                    retained_path: path,
                    origin,
                    packet_records,
                    import_source: "lightroom".into(),
                    association: Association::Unresolved,
                    supplement,
                },
                asset,
            })
        }
        fn run(&mut self) -> Result<ProjectionResult> {
            self.catalog
                .project_migration_file_metadata(Some(&self.source), &self.request)
        }
    }
    #[test]
    fn complete_embedded_projects_full_evidence_once_and_shares_with_copy() -> Result<()> {
        let mut t = Test::new(Origin::Embedded, Status::Complete, false, false)?;
        let master = VariantKey::master(&t.asset);
        let copy = t.catalog.create_edit_variant(&master, 0, "copy")?.key;
        let before = std::fs::read(&t._fixture.path)?;
        let done = t.run()?;
        assert_eq!(done.state, "metadata_retained");
        let packets = t
            .catalog
            .metadata_packets_for_image(&master, done.observation.unwrap())?;
        assert_eq!(packets[0].bytes, XML);
        assert!(
            !t.catalog
                .metadata_history_for_image(&copy, 0, 100)?
                .is_empty()
        );
        let revision = t.catalog.metadata_for_image(&master)?.revision;
        assert_eq!(
            t.catalog
                .project_migration_file_metadata(None, &t.request)?,
            done
        );
        assert_eq!(t.catalog.metadata_for_image(&master)?.revision, revision);
        assert_eq!(std::fs::read(&t._fixture.path)?, before);
        Ok(())
    }
    #[test]
    fn sidecar_confirmation_is_explicit_and_cannot_change_on_replay() -> Result<()> {
        let mut t = Test::new(Origin::AppendedXmp, Status::Complete, false, false)?;
        let done = t.run()?;
        assert_eq!(done.state, "ambiguous_metadata_retained");
        assert_eq!(
            t.catalog
                .metadata_for_image(&VariantKey::master(&t.asset))?
                .sources[0]
                .association,
            "ambiguous"
        );
        t.request.association = Association::Confirmed {
            reason: "reviewed exact source ownership".into(),
        };
        assert!(t.run().is_err());
        let mut other = Test::new(Origin::SidecarUpper, Status::Complete, false, false)?;
        other.request.association = Association::Confirmed {
            reason: "reviewed literal source association".into(),
        };
        assert_eq!(other.run()?.state, "metadata_retained");
        Ok(())
    }
    #[test]
    fn subset_wrong_file_and_hidden_roster_fail_without_native_change() -> Result<()> {
        let mut t = Test::new(Origin::Embedded, Status::Complete, false, false)?;
        t.request.packet_records.pop();
        assert!(t.run().is_err());
        let mut t = Test::new(Origin::Embedded, Status::Complete, false, true)?;
        assert!(t.run().is_err());
        assert_eq!(
            t.catalog
                .db
                .query_row("SELECT count(*) FROM migration_file_metadata", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        t.request.file.source.key = vec![Cell::Integer(999)];
        assert!(t.run().is_err());
        Ok(())
    }
    #[test]
    fn incomplete_is_retained_and_complete_supplement_is_separate() -> Result<()> {
        let mut t = Test::new(Origin::Embedded, Status::Malformed, true, false)?;
        let original = t.request.supplement.take();
        let old = t.run()?;
        assert_eq!(old.state, "retained_only");
        assert!(old.observation.is_none());
        t.request.supplement = original;
        let new = t.run()?;
        assert_eq!(new.state, "metadata_retained");
        assert_eq!(new.historical_status, Some(Status::Malformed));
        assert_eq!(
            t.catalog
                .db
                .query_row("SELECT count(*) FROM migration_file_metadata", [], |r| r
                    .get::<_, i64>(0))?,
            2
        );
        let forged = upload(
            &mut t.catalog,
            b"arbitrary unqualified summary",
            "wrong proof",
        )?;
        t.request.supplement = Some(forged.evidence);
        assert!(t.run().is_err());
        Ok(())
    }
    #[test]
    fn metadata_checkpoint_failure_rolls_back_observation_and_retries() -> Result<()> {
        let mut t = Test::new(Origin::Embedded, Status::Complete, false, false)?;
        t.catalog.db.execute_batch("CREATE TRIGGER fail_file_metadata BEFORE INSERT ON migration_file_metadata BEGIN SELECT RAISE(ABORT,'fixture interruption'); END;")?;
        assert!(t.run().is_err());
        assert!(
            t.catalog
                .metadata_history_for_image(&VariantKey::master(&t.asset), 0, 100)?
                .is_empty()
        );
        t.catalog
            .db
            .execute_batch("DROP TRIGGER fail_file_metadata;")?;
        t.run()?;
        Ok(())
    }
    #[test]
    fn origin_query_is_selected_bounded_and_keeps_case_distinctions() -> Result<()> {
        let t = Test::new(Origin::SidecarXmp, Status::Complete, false, false)?;
        assert_eq!(
            t.source
                .origin_packet_roster(
                    &t.request.file.source.capture_revision,
                    "file-id",
                    "sidecar_xmp"
                )?
                .len(),
            2
        );
        assert!(
            t.source
                .origin_packet_roster(
                    &t.request.file.source.capture_revision,
                    "file-id",
                    "sidecar_XMP"
                )?
                .is_empty()
        );
        assert!(
            t.source
                .origin_packet_roster(
                    &t.source.seal().excluded_revisions[0],
                    "file-id",
                    "sidecar_xmp"
                )
                .is_err()
        );
        assert!(
            t.source
                .origin_packet_roster(
                    &t.request.file.source.capture_revision,
                    "file-id",
                    "sidecar_%"
                )
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn interpretation_limits_keep_complete_custody_without_reading_oversized_bytes() -> Result<()> {
        let t = Test::new(Origin::Embedded, Status::Complete, false, false)?;
        let mut proof = Evidence::with_record_limit(2052)?;
        proof.source(&t.catalog.db, &t.request.file)?;
        let path = proof.record(&t.catalog.db, t.request.retained_path)?;
        let mut h = historical(
            &t.catalog.db,
            &mut proof,
            &t.request,
            "file-id",
            &path,
            true,
        )?;
        let packet = *h.packet_slots.get(&0).unwrap();
        let Field::Bytes(reference) = h.records[packet].1.fields.get_mut("raw").unwrap() else {
            panic!("raw must be external custody")
        };
        reference.bytes = PACKET as u64 + 1;
        assert!(reconstruct(&t.catalog.db, &h, Origin::Embedded)?.is_none());
        // No attempted read using this deliberately unadmitted larger descriptor.
        assert!(Evidence::with_record_limit(2053).is_err());
        assert!(Evidence::with_record_limit(2052).is_ok());
        Ok(())
    }
    #[test]
    fn supplemental_transport_never_supplies_hash_or_length_authority() -> Result<()> {
        let mut t = Test::new(Origin::Embedded, Status::Complete, false, false)?;
        let mut p = upload(&mut t.catalog, b"complete bytes", "hash negative")?;
        p.blake3 = "0".repeat(64);
        assert!(payload(&t.catalog, &p, META).is_err());
        p.blake3 = blake3::hash(b"complete bytes").to_hex().to_string();
        p.length += 1;
        assert!(payload(&t.catalog, &p, META).is_err());
        assert!(indices(&[1], 1).is_err());
        assert!(
            ranges(
                &[xmp_packets::ByteRange {
                    offset: 100000,
                    length: 1
                }],
                &SourceRevision {
                    length: 100000,
                    blake3: "a".repeat(64),
                    modified_unix_ns: None
                }
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn full_native_roster_ceiling_is_accepted_and_overflow_fails() -> Result<()> {
        let mut fixture = Fixture::new();
        let revision = fixture.revision().to_owned();
        fixture.edit(|db| {
            for i in 0..2048 {db.execute("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,detail) VALUES(?1,'many',?2,'hash',x'','{}')",params![revision,format!("embedded:{}:{}",if i<1024 {"packet"} else {"parse_input"},i%1024)]).unwrap();}
        });
        let source = fixture.open();
        assert_eq!(
            source
                .origin_packet_roster(&revision, "many", "embedded")?
                .len(),
            2048
        );
        drop(source);
        fixture.edit(|db|{db.execute("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,detail) VALUES(?1,'many','embedded:packet:extra','hash',x'','{}')",[&revision]).unwrap();});
        assert!(
            fixture
                .open()
                .origin_packet_roster(&revision, "many", "embedded")
                .is_err()
        );
        Ok(())
    }
}
