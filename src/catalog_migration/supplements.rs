//! Prepare separately qualified PSD successor evidence without opening originals.
//!
//! This uploads generic evidence only. Returned pins must still be admitted in the
//! externally approved selected seal; preparing a proof never changes that seal,
//! historical inspection status, or importer policy. Absolute paths recorded by
//! the probe are provenance only. Payload copies come from literal sibling names
//! beneath the caller's explicit root, so a complete proof tree may be relocated.
use super::{
    evidence,
    file_metadata::{Payload, SupplementInput, SupplementPacket, SupplementalProof},
};
use crate::{
    Catalog,
    lightroom::{
        migration_source::SupplementPin,
        source::{Source, reject_links},
    },
    storage_volume::NativePath,
    xmp_packets::{ByteRange, Container, Issue, SourceRevision, Status, Transformation},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Component, Path},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const DOCUMENT: usize = 4 * 1024 * 1024;
const PACKET: usize = 16 * 1024 * 1024;
const PAYLOAD: u64 = 64 * 1024 * 1024;
const COUNT: usize = 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub proof_root: NativePath,
    pub inspection_relative: NativePath,
    pub inspection_blake3: String,
    pub capture_revision: String,
    pub source_id: String,
    pub source_revision: SourceRevision,
    /// The original retained observation supplies this; the probe has no such field.
    pub historical_status: Status,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    pub pin: SupplementPin,
    /// Generic evidence ID of the normalized SupplementalProof, not an approval.
    pub evidence: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    baseline_equal: bool,
    qualified: bool,
    revision: String,
    source_id: String,
    recorded_source: SourceRevision,
    source_revision: SourceRevision,
    status: Status,
    issues: Vec<Issue>,
    packets: Vec<Raw>,
    parse_inputs: Vec<Decoded>,
    // Kept byte-exact in validation_document, never interpreted as I/O authority.
    source_path: serde_json::Value,
    work: serde_json::Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRef {
    blake3: String,
    bytes: u64,
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    attributes: BTreeMap<String, String>,
    container: Container,
    decoded_bytes: Option<u64>,
    digest: String,
    file: FileRef,
    group: String,
    origin: String,
    ranges: Vec<ByteRange>,
    raw_bytes: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decoded {
    decoded_bytes: u64,
    digest: String,
    file: FileRef,
    group: String,
    origin: String,
    packet_indices: Vec<usize>,
    raw_bytes: u64,
    transformation: Transformation,
}
fn digest(s: &str) -> Result<()> {
    ensure!(
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected lowercase BLAKE3 identity"
    );
    Ok(())
}
fn checkpoint(stop: &AtomicBool, deadline: Instant) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "supplement preparation stopped"
    );
    ensure!(Instant::now() < deadline, "supplement preparation deadline");
    Ok(())
}
fn path_bound(path: &NativePath) -> Result<()> {
    let n = match path {
        NativePath::UnixBytes(v) => v.len(),
        NativePath::WindowsWide(v) => v.len().checked_mul(2).context("path size")?,
    };
    ensure!(n > 0 && n <= 16384, "supplement path bound");
    Ok(())
}
fn read(
    path: &Path,
    maximum: usize,
    expected: &str,
    stop: &AtomicBool,
    deadline: Instant,
) -> Result<Vec<u8>> {
    digest(expected)?;
    checkpoint(stop, deadline)?;
    let mut source = Source::open(path, maximum as u64)?;
    source.lock(0, source.before.bytes.max(1))?;
    let length = usize::try_from(source.before.bytes)?;
    let mut bytes = vec![0; length];
    for chunk in bytes.chunks_mut(65536) {
        checkpoint(stop, deadline)?;
        source.file.read_exact(chunk)?;
    }
    source.verify()?;
    checkpoint(stop, deadline)?;
    ensure!(
        blake3::hash(&bytes).to_hex().as_str() == expected,
        "supplement file bytes differ from expected hash"
    );
    Ok(bytes)
}
fn file_ref(file: &FileRef, hash: &str, length: u64) -> Result<()> {
    digest(hash)?;
    ensure!(
        file.blake3 == hash && file.bytes == length && length <= PACKET as u64,
        "supplement payload descriptor differs"
    );
    ensure!(
        !file.path.is_empty() && file.path.len() <= 16384,
        "recorded payload path bounds"
    );
    Ok(())
}
fn upload(
    catalog: &mut Catalog,
    bytes: &[u8],
    stop: &AtomicBool,
    deadline: Instant,
) -> Result<Payload> {
    checkpoint(stop, deadline)?;
    let hash = blake3::hash(bytes).to_hex().to_string();
    // Identical bytes use the same immutable generic object across copy locations/retries.
    let descriptor = serde_json::to_vec(
        &serde_json::json!({"adapter":"qualified-psd-supplement-v1","blake3":hash,"bytes":bytes.len()}),
    )?;
    let mut state = catalog.begin_migration_evidence(&descriptor, bytes.len() as u64)?;
    // Incomplete evidence is intentionally not readable through the public API.
    // Compare its bounded committed chunk descriptors to the already verified
    // input, then check all actual stored bytes once the object is complete.
    let mut checked = 0usize;
    while (checked as u64) < state.committed {
        checkpoint(stop, deadline)?;
        let (hash, length): (String, i64) = catalog.db.query_row(
            "SELECT b.hash,b.length FROM migration_evidence_chunks c JOIN migration_evidence_blobs b ON b.hash=c.hash WHERE c.evidence=?1 AND c.offset=?2",
            rusqlite::params![state.id, i64::try_from(checked)?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let length = usize::try_from(length)?;
        ensure!(
            length > 0 && length <= evidence::CHUNK_BYTES,
            "existing chunk length bound"
        );
        let end = checked
            .checked_add(length)
            .context("evidence prefix overflow")?;
        let expected = bytes
            .get(checked..end)
            .context("evidence prefix exceeds payload")?;
        ensure!(
            end as u64 <= state.committed && blake3::hash(expected).to_hex().as_str() == hash,
            "existing generic supplement evidence bytes differ"
        );
        checked = end;
    }
    while !state.complete {
        checkpoint(stop, deadline)?;
        let start = usize::try_from(state.committed)?;
        let end = bytes.len().min(start + evidence::CHUNK_BYTES);
        state =
            catalog.append_migration_evidence(&state.id, state.committed, &bytes[start..end])?;
    }
    let mut offset = 0usize;
    while offset < bytes.len() {
        checkpoint(stop, deadline)?;
        let chunk = catalog.migration_evidence_chunk(&state.id, offset as u64)?;
        let end = offset
            .checked_add(chunk.len())
            .context("complete evidence overflow")?;
        ensure!(
            !chunk.is_empty() && bytes.get(offset..end) == Some(chunk.as_slice()),
            "complete generic evidence bytes differ"
        );
        offset = end;
    }
    Ok(Payload {
        evidence: state.id,
        length: bytes.len() as u64,
        blake3: hash,
    })
}
impl Catalog {
    /// One document, at most 1024 raw and 1024 decoded copies, 16 MiB per copy,
    /// 64 MiB per class plus 4 MiB document. Reads are in 64 KiB pieces; uploads
    /// <=1 MiB. Stop/deadline checks are cooperative, including writer boundaries.
    /// All copies validate before upload. Failed/cancelled uploads may leave only
    /// resumable generic evidence; no pin is returned until normalized proof completes.
    pub fn prepare_migration_supplement(
        &mut self,
        request: &Request,
        stop: &AtomicBool,
    ) -> Result<Prepared> {
        let deadline = Instant::now() + Duration::from_secs(120);
        path_bound(&request.proof_root)?;
        path_bound(&request.inspection_relative)?;
        digest(&request.capture_revision)?;
        digest(&request.source_revision.blake3)?;
        ensure!(
            !request.source_id.is_empty() && request.source_id.len() <= 4096,
            "supplement source ID bounds"
        );
        ensure!(
            request.historical_status == Status::Malformed,
            "qualified PSD supplement requires historical malformed observation"
        );
        let root = request.proof_root.to_path()?;
        let relative = request.inspection_relative.to_path()?;
        ensure!(
            root.is_absolute()
                && relative
                    .components()
                    .all(|p| matches!(p, Component::Normal(_)))
                && relative.file_name() == Some(std::ffi::OsStr::new("inspection.json")),
            "supplement root/relative path invalid"
        );
        reject_links(&root)?;
        ensure!(root.is_dir(), "supplement root is not a directory");
        let path = root.join(relative);
        let original = read(&path, DOCUMENT, &request.inspection_blake3, stop, deadline)?;
        let doc: Document = serde_json::from_slice(&original)?;
        ensure!(
            doc.qualified
                && doc.baseline_equal
                && doc.status == Status::Complete
                && doc.issues.is_empty(),
            "supplement inspection was not qualified complete with equal baseline"
        );
        ensure!(
            doc.revision == request.capture_revision
                && doc.source_id == request.source_id
                && doc.recorded_source == request.source_revision
                && doc.source_revision == request.source_revision,
            "supplement historical association differs"
        );
        // These are required fields in the probe format, retained without opening them.
        ensure!(
            doc.source_path.is_object() && doc.work.is_object(),
            "probe provenance shape differs"
        );
        ensure!(
            !doc.packets.is_empty()
                && doc.packets.len() <= COUNT
                && doc.parse_inputs.len() == doc.packets.len(),
            "supplement payload count/coverage bound"
        );
        let raw_total = doc.packets.iter().try_fold(0u64, |n, p| {
            n.checked_add(p.file.bytes)
                .context("raw byte total overflow")
        })?;
        let decoded_total = doc.parse_inputs.iter().try_fold(0u64, |n, p| {
            n.checked_add(p.file.bytes)
                .context("decoded byte total overflow")
        })?;
        ensure!(
            raw_total <= PAYLOAD && decoded_total <= PAYLOAD,
            "supplement class byte bound"
        );
        let directory = path.parent().context("inspection parent absent")?;
        let mut raw = Vec::new();
        let mut decoded = Vec::new();
        for (i, p) in doc.packets.iter().enumerate() {
            file_ref(&p.file, &p.digest, p.raw_bytes)?;
            ensure!(
                p.origin == format!("embedded:packet:{i}")
                    && p.decoded_bytes.is_none()
                    && p.container == Container::PsdResource1060,
                "PSD raw packet shape differs"
            );
            ensure!(
                p.ranges.len() == 1
                    && p.ranges[0].length == p.raw_bytes
                    && p.ranges[0].offset <= doc.source_revision.length
                    && p.raw_bytes <= doc.source_revision.length - p.ranges[0].offset,
                "PSD source range differs"
            );
            let bytes = read(
                &directory.join(format!("packet-{i}.bin")),
                usize::try_from(p.file.bytes)?,
                &p.digest,
                stop,
                deadline,
            )?;
            ensure!(
                bytes.len() as u64 == p.file.bytes,
                "raw payload length differs"
            );
            raw.push(bytes);
        }
        for (i, p) in doc.parse_inputs.iter().enumerate() {
            file_ref(&p.file, &p.digest, p.decoded_bytes)?;
            ensure!(
                p.origin == format!("embedded:parse_input:{i}")
                    && p.raw_bytes == 0
                    && p.transformation == Transformation::Identity
                    && p.packet_indices == vec![i],
                "PSD decoded packet shape differs"
            );
            let index = p.packet_indices[0];
            let packet = doc
                .packets
                .get(index)
                .context("decoded packet index bounds")?;
            ensure!(
                p.group == packet.group
                    && p.digest == packet.digest
                    && p.decoded_bytes == packet.raw_bytes,
                "PSD identity transformation differs"
            );
            let bytes = read(
                &directory.join(format!("parse-input-{i}.bin")),
                usize::try_from(p.file.bytes)?,
                &p.digest,
                stop,
                deadline,
            )?;
            ensure!(
                bytes.len() as u64 == p.file.bytes && bytes == raw[index],
                "decoded identity payload differs"
            );
            decoded.push(bytes);
        }
        let validation_document = upload(self, &original, stop, deadline)?;
        let mut packets = Vec::new();
        for (p, bytes) in doc.packets.into_iter().zip(raw) {
            packets.push(SupplementPacket {
                payload: upload(self, &bytes, stop, deadline)?,
                container: p.container,
                ranges: p.ranges,
                group: p.group,
                attributes: p.attributes,
            });
        }
        let mut parse_inputs = Vec::new();
        for (p, bytes) in doc.parse_inputs.into_iter().zip(decoded) {
            parse_inputs.push(SupplementInput {
                payload: upload(self, &bytes, stop, deadline)?,
                transformation: p.transformation,
                packet_indices: p.packet_indices,
                group: p.group,
            });
        }
        let proof = SupplementalProof {
            protocol: 1,
            revision: doc.revision,
            source_id: doc.source_id,
            origin: "embedded".into(),
            source_revision: doc.source_revision,
            historical_status: request.historical_status,
            status: doc.status,
            issues: doc.issues,
            validation_document,
            packets,
            parse_inputs,
        };
        let normalized = crate::lightroom::bounded_json(&proof, 8 * 1024 * 1024)?;
        let payload = upload(self, &normalized, stop, deadline)?;
        checkpoint(stop, deadline)?;
        Ok(Prepared {
            pin: SupplementPin {
                revision: proof.revision,
                source_id: proof.source_id,
                origin: proof.origin,
                source_revision: proof.source_revision,
                historical_status: proof.historical_status,
                proof_blake3: payload.blake3,
            },
            evidence: payload.evidence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    struct Fixture {
        _temp: tempfile::TempDir,
        root: std::path::PathBuf,
        request: Request,
        doc: Value,
    }
    impl Fixture {
        fn new() -> Result<Self> {
            let temp = tempfile::tempdir()?;
            let root = temp.path().canonicalize()?;
            std::fs::create_dir(root.join("00"))?;
            let bytes = b"<x:xmpmeta>retained exact PSD packet</x:xmpmeta>";
            let hash = blake3::hash(bytes).to_hex().to_string();
            let revision = SourceRevision {
                length: 1000,
                blake3: "b".repeat(64),
                modified_unix_ns: Some(17),
            };
            let doc = json!({"baseline_equal":true,"qualified":true,"revision":"a".repeat(64),"source_id":"original-lineage:source-key","recorded_source":revision,"source_revision":revision,"status":"Complete","issues":[],"source_path":{"encoding":"UnixBytes","units":[47,110,111,116,45,111,112,101,110,101,100]},"work":{"whole_file_hash_passes":1},"packets":[{"attributes":{},"container":"PsdResource1060","decoded_bytes":null,"digest":hash,"file":{"blake3":hash,"bytes":bytes.len(),"path":"/old/probe/00/packet-0.bin"},"group":"psd:12","origin":"embedded:packet:0","ranges":[{"offset":24,"length":bytes.len()}],"raw_bytes":bytes.len()}],"parse_inputs":[{"decoded_bytes":bytes.len(),"digest":hash,"file":{"blake3":hash,"bytes":bytes.len(),"path":"/old/probe/00/parse-input-0.bin"},"group":"psd:12","origin":"embedded:parse_input:0","packet_indices":[0],"raw_bytes":0,"transformation":"Identity"}]});
            for name in ["packet-0.bin", "parse-input-0.bin"] {
                std::fs::write(root.join("00").join(name), bytes)?;
            }
            let request = Request {
                proof_root: NativePath::from_path(&root),
                inspection_relative: NativePath::from_path(Path::new("00/inspection.json")),
                inspection_blake3: String::new(),
                capture_revision: "a".repeat(64),
                source_id: "original-lineage:source-key".into(),
                source_revision: revision,
                historical_status: Status::Malformed,
            };
            let mut f = Self {
                _temp: temp,
                root,
                request,
                doc,
            };
            f.save()?;
            Ok(f)
        }
        fn save(&mut self) -> Result<()> {
            let b = serde_json::to_vec_pretty(&self.doc)?;
            self.request.inspection_blake3 = blake3::hash(&b).to_hex().to_string();
            std::fs::write(self.root.join("00/inspection.json"), b)?;
            Ok(())
        }
        fn catalog(&self) -> Result<Catalog> {
            Catalog::open(self.root.join("destination"))
        }
    }
    fn bytes(c: &Catalog, id: &str) -> Result<Vec<u8>> {
        let s = c.migration_evidence(id)?;
        ensure!(s.complete, "incomplete fixture evidence");
        let mut b = Vec::new();
        while (b.len() as u64) < s.length {
            b.extend(c.migration_evidence_chunk(id, b.len() as u64)?);
        }
        Ok(b)
    }
    fn empty(c: &Catalog) -> Result<()> {
        ensure!(
            c.db.query_row("SELECT count(*) FROM migration_evidence", [], |r| r
                .get::<_, i64>(0))?
                == 0,
            "invalid proof uploaded evidence"
        );
        Ok(())
    }
    #[test]
    fn qualified_psd_replay_retains_original_and_payloads_offline() -> Result<()> {
        let f = Fixture::new()?;
        let mut c = f.catalog()?;
        let stop = AtomicBool::new(false);
        let original = std::fs::read(f.root.join("00/inspection.json"))?;
        let first = c.prepare_migration_supplement(&f.request, &stop)?;
        let count: i64 =
            c.db.query_row("SELECT count(*) FROM migration_evidence", [], |r| r.get(0))?;
        let second = c.prepare_migration_supplement(&f.request, &stop)?;
        assert_eq!(serde_json::to_vec(&first)?, serde_json::to_vec(&second)?);
        assert_eq!(
            count,
            c.db.query_row("SELECT count(*) FROM migration_evidence", [], |r| r
                .get::<_, i64>(0))?
        );
        drop(c);
        std::fs::remove_dir_all(f.root.join("00"))?;
        let c = f.catalog()?;
        let normalized = bytes(&c, &first.evidence)?;
        assert_eq!(
            first.pin.proof_blake3,
            blake3::hash(&normalized).to_hex().to_string()
        );
        let proof: SupplementalProof = serde_json::from_slice(&normalized)?;
        assert_eq!(bytes(&c, &proof.validation_document.evidence)?, original);
        for p in [&proof.packets[0].payload, &proof.parse_inputs[0].payload] {
            let b = bytes(&c, &p.evidence)?;
            assert_eq!(p.length, b.len() as u64);
            assert_eq!(p.blake3, blake3::hash(&b).to_hex().to_string());
        }
        assert_eq!(proof.historical_status, Status::Malformed);
        assert_eq!(proof.status, Status::Complete);
        assert_eq!(first.pin.source_revision, f.request.source_revision);
        Ok(())
    }
    #[test]
    fn generic_upload_checks_existing_prefix_and_resumes_arbitrary_chunks() -> Result<()> {
        for poisoned in [false, true] {
            let f = Fixture::new()?;
            let mut c = f.catalog()?;
            let expected = b"abcdef";
            let hash = blake3::hash(expected).to_hex().to_string();
            let descriptor = serde_json::to_vec(
                &json!({"adapter":"qualified-psd-supplement-v1","blake3":hash,"bytes":expected.len()}),
            )?;
            let state = c.begin_migration_evidence(&descriptor, expected.len() as u64)?;
            c.append_migration_evidence(&state.id, 0, if poisoned { b"xx" } else { b"ab" })?;
            let result = upload(
                &mut c,
                expected,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(10),
            );
            if poisoned {
                assert!(result.is_err());
                assert_eq!(c.migration_evidence(&state.id)?.committed, 2);
            } else {
                let payload = result?;
                assert_eq!(bytes(&c, &payload.evidence)?, expected);
                assert_eq!(payload.blake3, hash);
            }
        }
        Ok(())
    }
    #[test]
    fn changed_bytes_or_advertised_length_fail_before_upload() -> Result<()> {
        for name in ["inspection.json", "packet-0.bin", "parse-input-0.bin"] {
            let f = Fixture::new()?;
            let mut c = f.catalog()?;
            std::fs::write(f.root.join("00").join(name), b"changed")?;
            assert!(
                c.prepare_migration_supplement(&f.request, &AtomicBool::new(false))
                    .is_err()
            );
            empty(&c)?;
        }
        let mut f = Fixture::new()?;
        let mut c = f.catalog()?;
        f.doc["packets"][0]["file"]["bytes"] = json!(1);
        f.doc["packets"][0]["raw_bytes"] = json!(1);
        f.doc["packets"][0]["ranges"][0]["length"] = json!(1);
        f.save()?;
        assert!(
            c.prepare_migration_supplement(&f.request, &AtomicBool::new(false))
                .is_err()
        );
        empty(&c)
    }
    #[test]
    fn exact_association_qualification_and_complete_coverage_required() -> Result<()> {
        for pointer in [
            "/qualified",
            "/baseline_equal",
            "/recorded_source/length",
            "/source_revision/length",
            "/revision",
            "/source_id",
            "/status",
            "/parse_inputs/0/packet_indices",
            "/packets/0/container",
        ] {
            let mut f = Fixture::new()?;
            let mut c = f.catalog()?;
            *f.doc.pointer_mut(pointer).unwrap() = match pointer {
                "/qualified" | "/baseline_equal" => json!(false),
                "/recorded_source/length" | "/source_revision/length" => json!(1001),
                "/parse_inputs/0/packet_indices" => json!([1]),
                _ => json!("wrong"),
            };
            f.save()?;
            assert!(
                c.prepare_migration_supplement(&f.request, &AtomicBool::new(false))
                    .is_err(),
                "{pointer}"
            );
            empty(&c)?;
        }
        Ok(())
    }
    #[test]
    fn traversal_foreign_path_stop_and_unknown_request_denied() -> Result<()> {
        let f = Fixture::new()?;
        let mut c = f.catalog()?;
        for relative in [
            "../00/inspection.json",
            "/00/inspection.json",
            "00/other.json",
        ] {
            let mut r = f.request.clone();
            r.inspection_relative = NativePath::from_path(Path::new(relative));
            assert!(
                c.prepare_migration_supplement(&r, &AtomicBool::new(false))
                    .is_err()
            );
        }
        let mut r = f.request.clone();
        #[cfg(unix)]
        {
            r.proof_root = NativePath::WindowsWide(vec![67, 58, 92]);
        }
        #[cfg(windows)]
        {
            r.proof_root = NativePath::UnixBytes(b"/foreign".to_vec());
        }
        assert!(
            c.prepare_migration_supplement(&r, &AtomicBool::new(false))
                .is_err()
        );
        assert!(
            c.prepare_migration_supplement(&f.request, &AtomicBool::new(true))
                .is_err()
        );
        let mut v = serde_json::to_value(&f.request)?;
        v["approval"] = json!(true);
        assert!(serde_json::from_value::<Request>(v).is_err());
        empty(&c)
    }
    #[cfg(unix)]
    #[test]
    fn linked_payload_or_ancestor_is_never_followed() -> Result<()> {
        for ancestor in [false, true] {
            let mut f = Fixture::new()?;
            let mut c = f.catalog()?;
            if ancestor {
                std::fs::rename(f.root.join("00"), f.root.join("real"))?;
                std::os::unix::fs::symlink("real", f.root.join("00"))?;
            } else {
                std::fs::rename(f.root.join("00/packet-0.bin"), f.root.join("00/real.bin"))?;
                std::os::unix::fs::symlink("real.bin", f.root.join("00/packet-0.bin"))?;
            }
            // The expected document remains valid; only the filesystem mapping changed.
            assert!(
                c.prepare_migration_supplement(&f.request, &AtomicBool::new(false))
                    .is_err()
            );
            empty(&c)?;
            f.request.historical_status = Status::Complete;
            assert!(
                c.prepare_migration_supplement(&f.request, &AtomicBool::new(false))
                    .is_err()
            );
        }
        Ok(())
    }
}
