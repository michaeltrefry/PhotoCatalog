//! Durable raw capture custody, separate from inspection and image projection.
//!
//! Only a completed, authorized Captures record can supply a manifest member.
//! Its saved source locator is provenance, never an I/O path. The coordinator
//! supplies a separately sealed root/relative mapping and the *copy's* identity.
//! Opening hashes that entire file once. Each later step verifies identity and
//! copies at most one bounded chunk; it does not repeat the whole-file hash.
//! Use an isolated worker and externally exclude non-cooperative writers: POSIX
//! byte locks are process-scoped and can be released by another same-inode close.

use super::{
    evidence::{self, EvidenceState, PreparedChunk},
    retention,
};
use crate::{
    Catalog,
    catalog_writer::Priority,
    lightroom::{
        capture::{Artifact, Manifest},
        migration_source::{Collection, FileIdentity},
        source::{Source, reject_links},
    },
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Component, PathBuf},
    time::{Duration, Instant},
};

pub(crate) mod descriptor_json;
mod preparation;
pub use preparation::{MappingPreparation, prepare_manifest_artifact, prepare_mapping};

const DESCRIPTOR_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactMapping {
    pub root: NativePath,
    pub relative: NativePath,
    /// The current sealed raw copy, not the original inode in Artifact.revision.
    pub copy_identity: FileIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRequest {
    pub retained_capture_record: i64,
    pub member_index: usize,
    pub mapping: ArtifactMapping,
}

#[derive(Clone, Copy, Debug)]
pub struct ArtifactLimits {
    pub maximum_bytes: u64,
    pub open_deadline_ms: u64,
    pub chunk_deadline_ms: u64,
    pub chunk_bytes: usize,
}
impl ArtifactLimits {
    pub(crate) fn validate(self) -> Result<()> {
        ensure!(
            self.maximum_bytes > 0 && self.maximum_bytes <= i64::MAX as u64,
            "artifact byte admission limit"
        );
        ensure!(
            (1..=3_600_000).contains(&self.open_deadline_ms)
                && (1..=120_000).contains(&self.chunk_deadline_ms),
            "artifact deadline bounds"
        );
        ensure!(
            (1..=evidence::CHUNK_BYTES).contains(&self.chunk_bytes),
            "artifact chunk bound"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDescriptor {
    pub protocol: u32,
    pub request: ArtifactRequest,
    pub selected_input: String,
    pub capture_revision: String,
    pub manifest_blake3: String,
    pub artifact: Artifact,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS migration_artifacts(
        retained_capture_record INTEGER NOT NULL REFERENCES migration_retained_records(sequence),
        member_index INTEGER NOT NULL CHECK(member_index>=0),
        descriptor BLOB NOT NULL CHECK(length(descriptor)<=65536),
        evidence TEXT NOT NULL UNIQUE REFERENCES migration_evidence(id),
        PRIMARY KEY(retained_capture_record,member_index));",
    )?;
    Ok(())
}

fn mapping_path(mapping: &ArtifactMapping) -> Result<PathBuf> {
    mapping_path_parts(&mapping.root, &mapping.relative)
}
fn mapping_path_parts(root: &NativePath, relative: &NativePath) -> Result<PathBuf> {
    let (root_path, relative_path) = mapping_native_paths(root, relative)?;
    reject_links(&root_path)?;
    ensure!(
        fs::symlink_metadata(&root_path)?.is_dir(),
        "artifact sealed root is not a directory"
    );
    Ok(root_path.join(relative_path))
}
fn mapping_native_paths(root: &NativePath, relative: &NativePath) -> Result<(PathBuf, PathBuf)> {
    for native in [root, relative] {
        let units = match native {
            NativePath::UnixBytes(v) => v.len(),
            NativePath::WindowsWide(v) => v.len(),
        };
        ensure!((1..=32768).contains(&units), "artifact mapping path length");
    }
    let root_path = root.to_path()?;
    let relative_path = relative.to_path()?;
    ensure!(
        root_path.is_absolute() && !relative_path.is_absolute(),
        "artifact mapping root/relative shape"
    );
    ensure!(
        relative_path
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
            && relative_path.components().count() <= 256,
        "artifact mapping contains non-normal component"
    );
    // components() normalizes a/./b; reject lexical dot components as well.
    match relative {
        NativePath::UnixBytes(v) => ensure!(
            v.split(|b| *b == b'/')
                .all(|p| !p.is_empty() && p != b"." && p != b".."),
            "artifact relative path component"
        ),
        NativePath::WindowsWide(v) => ensure!(
            v.split(|b| *b == 47 || *b == 92)
                .all(|p| !p.is_empty() && p != [46] && p != [46, 46] && !p.contains(&58)),
            "artifact relative path component"
        ),
    }
    Ok((root_path, relative_path))
}

/// Only the owned raw Source epoch uses this allocation path. The public/CLI
/// mapping route keeps its existing behavior. Grants occur before Source locks.
fn admitted_mapping_path(
    mapping: &ArtifactMapping,
    admit: &mut dyn FnMut(usize) -> Result<()>,
) -> Result<PathBuf> {
    fn add(a: usize, b: usize) -> Result<usize> {
        a.checked_add(b)
            .context("artifact path allocation overflow")
    }
    fn mul(a: usize, b: usize) -> Result<usize> {
        a.checked_mul(b)
            .context("artifact path allocation overflow")
    }
    for native in [&mapping.root, &mapping.relative] {
        let units = match native {
            NativePath::UnixBytes(v) => v.len(),
            NativePath::WindowsWide(v) => v.len(),
        };
        ensure!((1..=32768).contains(&units), "artifact mapping path length");
        #[cfg(unix)]
        let bytes = units;
        #[cfg(windows)]
        let bytes = add(mul(units, 3)?, mul(units, 6)?.max(8))?;
        admit(bytes)?;
    }
    let (root, relative) = mapping_native_paths(&mapping.root, &mapping.relative)?;
    #[cfg(windows)]
    Source::check_prepared_directory(&root, admit)?;
    #[cfg(unix)]
    {
        // Original-prefix PathBuf growth and transient CString used by std FS.
        admit(add(mul(root.as_os_str().len(), 4)?, 9)?)?;
        reject_links(&root)?;
        ensure!(
            fs::symlink_metadata(&root)?.is_dir(),
            "artifact sealed root is not a directory"
        );
    }
    let capacity = add(add(root.as_os_str().len(), relative.as_os_str().len())?, 1)?;
    admit(capacity)?;
    let mut original = std::ffi::OsString::with_capacity(capacity);
    original.push(&root);
    // Native component validation above has excluded absolute/non-normal relative
    // paths. Raw append avoids PathBuf::push's verbatim Vec/rebuild allocations.
    #[cfg(unix)]
    let separator = "/";
    #[cfg(windows)]
    let separator = "\\";
    if !original.as_encoded_bytes().ends_with(separator.as_bytes()) {
        original.push(separator);
    }
    original.push(&relative);
    #[cfg(unix)]
    // Source.path clone + prefix growth/realloc + std CString scratch. Retained
    // until reader reap, reused during serial revision checks.
    admit(add(mul(original.len(), 5)?, 9)?)?;
    Ok(original.into())
}

fn descriptor(db: &Connection, request: &ArtifactRequest) -> Result<ArtifactDescriptor> {
    #[cfg(all(test, feature = "internal-capacity-probes"))]
    let _capacity_phase = crate::capacity_probes::phase(crate::capacity_probes::DESCRIPTOR_RECORD);
    ensure!(
        request.retained_capture_record > 0 && i64::try_from(request.member_index).is_ok(),
        "artifact member selector bounds"
    );
    let record = retention::selected_capture(db, request.retained_capture_record)?;
    ensure!(
        record.collection == Collection::Captures,
        "artifact authority must be a retained Captures record"
    );
    #[cfg(all(test, feature = "internal-capacity-probes"))]
    crate::capacity_probes::observe(crate::capacity_probes::DESCRIPTOR_RECORD, 0);
    let bytes = retention::field_bytes(
        db,
        request.retained_capture_record,
        &record,
        "manifest",
        crate::lightroom::MANIFEST_BYTES,
    )?;
    #[cfg(all(test, feature = "internal-capacity-probes"))]
    crate::capacity_probes::observe(crate::capacity_probes::DESCRIPTOR_BYTES, bytes.capacity());
    let manifest = crate::lightroom::migration_source::manifest_json::decode(&bytes)?;
    #[cfg(all(test, feature = "internal-capacity-probes"))]
    crate::capacity_probes::observe(
        crate::capacity_probes::DESCRIPTOR_MANIFEST,
        crate::capacity_probes::manifest(&manifest),
    );

    ensure!(
        manifest.protocol == 1
            && manifest.state == "captured"
            && manifest.revision_id.as_deref() == Some(record.revision.as_str())
            && crate::lightroom::json_digest(&manifest.artifacts)? == record.revision,
        "retained artifact manifest revision differs"
    );
    let (input, seal): (Option<String>, Option<Vec<u8>>) = db.query_row("SELECT CASE WHEN typeof(i.id)='text' AND length(CAST(i.id AS BLOB))=64 THEN i.id END,CASE WHEN typeof(i.seal)='blob' AND length(i.seal)<=?2 THEN i.seal END FROM migration_retained_records r JOIN migration_retention i ON i.id=r.input WHERE r.sequence=?1", params![request.retained_capture_record,crate::lightroom::MANIFEST_BYTES as i64], |r| Ok((r.get(0)?,r.get(1)?)))?;
    let input = input.context("retained input identity storage type/64-byte admission")?;
    let seal = seal.context("retained seal storage type/byte admission limit")?;
    let seal = crate::lightroom::migration_source::seal_json::decode(
        &seal,
        crate::lightroom::MANIFEST_BYTES,
        &|| false,
    )?;
    #[cfg(all(test, feature = "internal-capacity-probes"))]
    crate::capacity_probes::observe(
        crate::capacity_probes::DESCRIPTOR_SEAL,
        crate::capacity_probes::seal(&seal),
    );
    let selected = seal
        .selected
        .iter()
        .find(|s| s.revision == record.revision)
        .context("excluded artifact capture")?;
    let manifest_blake3 = blake3::hash(&bytes).to_hex().to_string();
    ensure!(
        selected.manifest_blake3 == manifest_blake3,
        "retained selected manifest digest differs"
    );
    let artifact = manifest
        .artifacts
        .get(request.member_index)
        .context("unknown manifest artifact member")?
        .clone();
    ensure!(
        artifact.blake3.len() == 64
            && artifact
                .blake3
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid artifact BLAKE3"
    );
    ensure!(
        artifact.revision.bytes == request.mapping.copy_identity.bytes,
        "raw copy length differs from manifest artifact"
    );
    let result = ArtifactDescriptor {
        protocol: 1,
        request: request.clone(),
        selected_input: input,
        capture_revision: record.revision,
        manifest_blake3,
        artifact,
    };
    #[cfg(all(test, feature = "internal-capacity-probes"))]
    crate::capacity_probes::observe(
        crate::capacity_probes::DESCRIPTOR_CLONES,
        crate::capacity_probes::artifact(&result.artifact)
            + crate::capacity_probes::path(&result.request.mapping.root)
            + crate::capacity_probes::path(&result.request.mapping.relative)
            + crate::capacity_probes::revision(&result.request.mapping.copy_identity),
    );
    crate::lightroom::bounded_json(&result, DESCRIPTOR_LIMIT)?;
    Ok(result)
}

fn existing(
    db: &Connection,
    descriptor: &[u8],
    request: &ArtifactRequest,
) -> Result<Option<String>> {
    let previous: Option<(bool,String)> = db.query_row("SELECT descriptor,evidence FROM migration_artifacts WHERE retained_capture_record=?1 AND member_index=?2", params![request.retained_capture_record,i64::try_from(request.member_index)?], |r| Ok((evidence::retained_descriptor(r, 0)? == descriptor, evidence::retained_identity(r, 1)?))).optional()?;
    if let Some((matches, id)) = previous {
        ensure!(
            matches,
            "artifact custody mapping or provenance differs; explicit reconciliation required"
        );
        return Ok(Some(id));
    }
    Ok(None)
}

/// Cannot be constructed from public digest claims. Catalog admission resolves
/// the exact selected retained manifest before any mapped artifact is opened.
pub struct ArtifactReader {
    source: Source,
    #[cfg(windows)]
    _write_lease: fs::File,
    descriptor: ArtifactDescriptor,
    encoded: Vec<u8>,
    limits: ArtifactLimits,
    poisoned: bool,
}
impl ArtifactReader {
    pub(crate) fn open_descriptor(
        descriptor: ArtifactDescriptor,
        limits: ArtifactLimits,
        stop: &dyn Fn() -> bool,
        protected: &[crate::lightroom_migration_worker::identity::FileKey],
    ) -> Result<Self> {
        Self::open_descriptor_impl(descriptor, limits, stop, protected, None)
    }
    pub(crate) fn open_owned_descriptor(
        descriptor: ArtifactDescriptor,
        limits: ArtifactLimits,
        stop: &dyn Fn() -> bool,
        protected: &[crate::lightroom_migration_worker::identity::FileKey],
        admit: &mut dyn FnMut(usize) -> Result<()>,
    ) -> Result<Self> {
        Self::open_descriptor_impl(descriptor, limits, stop, protected, Some(admit))
    }
    fn open_descriptor_impl(
        descriptor: ArtifactDescriptor,
        limits: ArtifactLimits,
        stop: &dyn Fn() -> bool,
        protected: &[crate::lightroom_migration_worker::identity::FileKey],
        mut admit: Option<&mut dyn FnMut(usize) -> Result<()>>,
    ) -> Result<Self> {
        limits.validate()?;
        ensure!(!stop(), "artifact custody stopped");
        ensure!(protected.len() <= 4096, "artifact protected identity bound");
        let deadline = Instant::now() + Duration::from_millis(limits.open_deadline_ms);
        let encoded = crate::lightroom::bounded_json(&descriptor, DESCRIPTOR_LIMIT)?;
        let request = &descriptor.request;
        ensure!(
            descriptor.protocol == 1
                && request.retained_capture_record > 0
                && i64::try_from(request.member_index).is_ok(),
            "artifact descriptor protocol/member bounds"
        );
        ensure!(
            descriptor.artifact.revision.bytes == request.mapping.copy_identity.bytes,
            "artifact copy length differs from descriptor"
        );
        for hash in [
            &descriptor.selected_input,
            &descriptor.capture_revision,
            &descriptor.manifest_blake3,
            &descriptor.artifact.blake3,
        ] {
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "artifact descriptor digest bounds"
            );
        }
        ensure!(
            descriptor.artifact.revision.bytes <= limits.maximum_bytes,
            "artifact exceeds declared maximum bytes"
        );
        let path = if let Some(admit) = admit.as_mut() {
            admitted_mapping_path(&request.mapping, *admit)?
        } else {
            mapping_path(&request.mapping)?
        };
        #[cfg(windows)]
        let legacy_lease = if admit.is_none() {
            use std::os::windows::fs::OpenOptionsExt;
            reject_links(&path)?;
            Some(
                fs::OpenOptions::new()
                    .read(true)
                    .share_mode(1)
                    .open(&path)?,
            )
        } else {
            None
        };
        #[cfg(windows)]
        let mut source = if let Some(admit) = admit.as_mut() {
            Source::open_prepared(
                &path,
                limits.maximum_bytes,
                *admit,
                crate::lightroom::source::closed_path::file_path,
            )?
        } else {
            Source::open(&path, limits.maximum_bytes)?
        };
        #[cfg(not(windows))]
        let mut source = Source::open(&path, limits.maximum_bytes)?;
        #[cfg(windows)]
        let lease = match legacy_lease {
            Some(lease) => lease,
            None => {
                use std::os::windows::fs::OpenOptionsExt;
                fs::OpenOptions::new()
                    .read(true)
                    .share_mode(1)
                    .open(source.prepared_path())?
            }
        };
        ensure!(
            source.before == request.mapping.copy_identity,
            "artifact sealed copy identity differs"
        );
        let key = crate::lightroom_migration_worker::identity::FileKey::of(&source.file)?;
        ensure!(
            !protected.contains(&key),
            "artifact aliases a protected object"
        );
        #[cfg(windows)]
        ensure!(
            crate::lightroom_migration_worker::identity::FileKey::of(&lease)? == key,
            "artifact deny-write handle differs"
        );
        source.lock(0, 0)?;
        let mut remaining = source.before.bytes;
        let mut hash = blake3::Hasher::new();
        let mut buffer = [0; 128 * 1024];
        while remaining > 0 {
            ensure!(
                Instant::now() < deadline && !stop(),
                "artifact opening deadline/stop"
            );
            let size = remaining.min(buffer.len() as u64) as usize;
            source.file.read_exact(&mut buffer[..size])?;
            hash.update(&buffer[..size]);
            remaining -= size as u64;
        }
        source.verify()?;
        ensure!(
            hash.finalize().to_hex().as_str() == descriptor.artifact.blake3,
            "artifact content differs from retained manifest"
        );
        ensure!(
            Instant::now() < deadline && !stop(),
            "artifact opening deadline/stop"
        );
        Ok(ArtifactReader {
            source,
            #[cfg(windows)]
            _write_lease: lease,
            descriptor,
            encoded,
            limits,
            poisoned: false,
        })
    }

    pub fn descriptor(&self) -> &ArtifactDescriptor {
        &self.descriptor
    }
    fn verify(&mut self) -> Result<()> {
        ensure!(!self.poisoned, "artifact reader previously invalidated");
        let result = self.source.verify();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn chunk(&mut self, offset: u64, stop: &dyn Fn() -> bool) -> Result<Vec<u8>> {
        self.verify()?;
        ensure!(!stop(), "artifact custody stopped");
        let deadline = Instant::now() + Duration::from_millis(self.limits.chunk_deadline_ms);
        ensure!(
            offset <= self.source.before.bytes,
            "artifact offset exceeds source"
        );
        let size = (self.source.before.bytes - offset).min(self.limits.chunk_bytes as u64) as usize;
        let mut bytes = vec![0; size];
        self.source.file.seek(SeekFrom::Start(offset))?;
        self.source.file.read_exact(&mut bytes)?;
        self.verify()?;
        ensure!(
            Instant::now() < deadline && !stop(),
            "artifact chunk deadline/stop"
        );
        Ok(bytes)
    }
}

/// Internal transport implementations must bind all returned bytes to the
/// admitted descriptor and retain their reader epoch through transaction drain.
/// Public callers cannot construct a reader from untrusted digest claims.
pub(crate) trait ArtifactRead {
    fn descriptor(&self) -> &ArtifactDescriptor;
    fn encoded(&self) -> &[u8];
    fn length(&self) -> u64;
    fn verify(&mut self) -> Result<()>;
    fn chunk(&mut self, offset: u64, stop: &dyn Fn() -> bool) -> Result<Vec<u8>>;
}
impl ArtifactRead for ArtifactReader {
    fn descriptor(&self) -> &ArtifactDescriptor {
        &self.descriptor
    }
    fn encoded(&self) -> &[u8] {
        &self.encoded
    }
    fn length(&self) -> u64 {
        self.source.before.bytes
    }
    fn verify(&mut self) -> Result<()> {
        ArtifactReader::verify(self)
    }
    fn chunk(&mut self, offset: u64, stop: &dyn Fn() -> bool) -> Result<Vec<u8>> {
        ArtifactReader::chunk(self, offset, stop)
    }
}

impl Catalog {
    /// Full-file verification is outside the destination writer. A resumed
    /// reader repeats this admission hash once; individual chunks do not.
    pub fn open_migration_artifact(
        &self,
        request: ArtifactRequest,
        limits: ArtifactLimits,
        stop: &dyn Fn() -> bool,
    ) -> Result<ArtifactReader> {
        limits.validate()?;
        ensure!(!stop(), "artifact custody stopped");
        let descriptor = self.migration_artifact_reader_descriptor(&request)?;
        ArtifactReader::open_descriptor(descriptor, limits, stop, &[])
    }
    /// Resolve immutable destination authority without opening the raw copy.
    pub(crate) fn migration_artifact_reader_descriptor(
        &self,
        request: &ArtifactRequest,
    ) -> Result<ArtifactDescriptor> {
        let descriptor = descriptor(&self.db, request)?;
        let encoded = crate::lightroom::bounded_json(&descriptor, DESCRIPTOR_LIMIT)?;
        existing(&self.db, &encoded, request)?;
        Ok(descriptor)
    }

    pub fn begin_migration_artifact(
        &mut self,
        reader: &mut ArtifactReader,
    ) -> Result<EvidenceState> {
        self.begin_migration_artifact_reader(reader)
    }
    pub(crate) fn begin_migration_artifact_reader(
        &mut self,
        reader: &mut dyn ArtifactRead,
    ) -> Result<EvidenceState> {
        reader.verify()?;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Recheck immutable destination authority: a reader from another catalog
        // cannot authorize a coincidentally equal retained-record sequence.
        ensure!(
            crate::lightroom::bounded_json(
                &descriptor(&tx, &reader.descriptor().request)?,
                DESCRIPTOR_LIMIT
            )? == reader.encoded(),
            "artifact destination authority differs"
        );
        existing(&tx, reader.encoded(), &reader.descriptor().request)?;
        reader.verify()?;
        let result = evidence::begin_owned(
            &tx,
            reader.encoded(),
            reader.length(),
            evidence::Authority::CapturedArtifact,
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO migration_artifacts VALUES(?1,?2,?3,?4)",
            params![
                reader.descriptor().request.retained_capture_record,
                i64::try_from(reader.descriptor().request.member_index)?,
                reader.encoded(),
                result.id
            ],
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// One bounded read/compress then one atomic destination transaction. Stop
    /// and deadline checks are checkpoints, not hard preemption of filesystem I/O.
    pub fn step_migration_artifact(
        &mut self,
        reader: &mut ArtifactReader,
        stop: &dyn Fn() -> bool,
    ) -> Result<EvidenceState> {
        self.step_migration_artifact_reader(reader, stop)
    }
    pub(crate) fn step_migration_artifact_reader(
        &mut self,
        reader: &mut dyn ArtifactRead,
        stop: &dyn Fn() -> bool,
    ) -> Result<EvidenceState> {
        reader.verify()?;
        ensure!(!stop(), "artifact custody stopped");
        let id = existing(&self.db, reader.encoded(), &reader.descriptor().request)?
            .context("artifact custody has not begun")?;
        let before = self.migration_evidence(&id)?;
        if before.complete {
            return Ok(before);
        }
        let bytes = reader.chunk(before.committed, stop)?;
        let prepared = PreparedChunk::new(&bytes)?;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            existing(&tx, reader.encoded(), &reader.descriptor().request)?.as_deref()
                == Some(id.as_str()),
            "artifact custody changed"
        );
        #[cfg(all(test, feature = "internal-capacity-probes"))]
        crate::capacity_probes::observe(
            crate::capacity_probes::CHUNK_VERIFY,
            bytes.capacity() + prepared.probe_capacity(),
        );
        reader.verify()?;
        ensure!(!stop(), "artifact custody stopped before commit");
        let result = evidence::append_owned(
            &tx,
            &id,
            before.committed,
            &prepared,
            evidence::Authority::CapturedArtifact,
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub fn migration_artifact(
        &self,
        retained_capture_record: i64,
        member_index: usize,
    ) -> Result<(ArtifactDescriptor, EvidenceState)> {
        let (bytes,id):(Vec<u8>,String)=self.db.query_row("SELECT descriptor,evidence FROM migration_artifacts WHERE retained_capture_record=?1 AND member_index=?2",params![retained_capture_record,i64::try_from(member_index)?],|r|Ok((evidence::retained_descriptor(r, 0)?.to_vec(),evidence::retained_identity(r, 1)?)))?;
        ensure!(bytes.len() <= DESCRIPTOR_LIMIT, "artifact descriptor limit");
        ensure!(
            self.migration_evidence_descriptor(&id)? == bytes,
            "artifact evidence descriptor differs"
        );
        Ok((
            serde_json::from_slice(&bytes)?,
            self.migration_evidence(&id)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lightroom::migration_source::{MigrationSource, ReadLimits, tests::Fixture};
    use std::path::Path;

    struct RawFixture {
        _root: tempfile::TempDir,
        inspection: Fixture,
        catalog: PathBuf,
        requests: Vec<ArtifactRequest>,
        expected: Vec<Vec<u8>>,
    }
    impl RawFixture {
        fn new(length: usize) -> Result<(Self, Catalog)> {
            Self::with_manifest(length, |_| {})
        }
        fn with_manifest(
            length: usize,
            customize: impl FnOnce(&mut crate::lightroom::capture::Manifest),
        ) -> Result<(Self, Catalog)> {
            let root = tempfile::tempdir()?;
            let absolute = fs::canonicalize(root.path())?;
            let raw = absolute.join("sealed");
            fs::create_dir(&raw)?;
            let expected = vec![
                (0..length).map(|n| (n % 251) as u8).collect::<Vec<_>>(),
                vec![0, 255, 0, 129, 3],
            ];
            let mut inspection = Fixture::new();
            let source = inspection.open();
            let mut manifest = source.capture_manifest(inspection.revision())?;
            drop(source);
            let mut mappings = Vec::new();
            manifest.artifacts.clear();
            for (index, bytes) in expected.iter().enumerate() {
                let relative = format!("member-{index}.opaque");
                let path = raw.join(&relative);
                fs::write(&path, bytes)?;
                let copy = Source::open(&path, u64::MAX)?.before.clone();
                mappings.push(ArtifactMapping {
                    root: NativePath::from_path(&raw),
                    relative: NativePath::from_path(Path::new(&relative)),
                    copy_identity: copy.clone(),
                });
                let mut original = copy;
                original.object = format!("historical-original-{index}");
                original.changed = "historical stamp".into();
                manifest.artifacts.push(Artifact {
                    source: NativePath::from_path(Path::new("/never/open/original")),
                    role: if index == 0 {
                        "main"
                    } else {
                        "opaque_auxiliary"
                    }
                    .into(),
                    relative: NativePath::from_path(Path::new(&relative)),
                    stored: format!("raw/{relative}"),
                    revision: original,
                    blake3: blake3::hash(bytes).to_hex().to_string(),
                });
            }
            customize(&mut manifest);
            let old = inspection.revision().to_owned();
            let revision = crate::lightroom::json_digest(&manifest.artifacts)?;
            manifest.revision_id = Some(revision.clone());
            let encoded = serde_json::to_string(&manifest)?;
            inspection.seal.selected[0].revision = revision.clone();
            inspection.seal.selected[0].manifest_blake3 =
                blake3::hash(encoded.as_bytes()).to_hex().to_string();
            let approval = b"synthetic selected raw custody approval";
            inspection.seal.approval.document_blake3 = blake3::hash(approval).to_hex().to_string();
            inspection.edit(|db| {
                for table in ["captures", "tables", "rows", "paths", "family_choices"] {
                    db.execute(
                        &format!("UPDATE {table} SET revision=? WHERE revision=?"),
                        params![revision, old],
                    )
                    .unwrap();
                }
                db.execute(
                    "UPDATE captures SET manifest=? WHERE revision=?",
                    params![encoded, revision],
                )
                .unwrap();
            });
            let source = MigrationSource::open(inspection.seal.clone(), ReadLimits::default())?;
            let catalog_path = absolute.join("destination");
            let mut catalog = Catalog::open(&catalog_path)?;
            install(&catalog.db)?;
            catalog.begin_migration_retention(&source, approval)?;
            for _ in 0..100 {
                if catalog.step_migration_retention(&source)?.complete {
                    break;
                }
            }
            ensure!(
                catalog
                    .migration_retention_progress(source.binding_blake3())?
                    .complete,
                "fixture retention incomplete"
            );
            let captures = catalog.retained_migration_records(
                source.binding_blake3(),
                &revision,
                Collection::Captures,
                0,
                10,
            )?;
            ensure!(captures.len() == 1, "fixture capture count");
            let requests = mappings
                .into_iter()
                .enumerate()
                .map(|(member_index, mapping)| ArtifactRequest {
                    retained_capture_record: captures[0].0,
                    member_index,
                    mapping,
                })
                .collect();
            drop(source);
            Ok((
                Self {
                    _root: root,
                    inspection,
                    catalog: catalog_path,
                    requests,
                    expected,
                },
                catalog,
            ))
        }
        fn limits() -> ArtifactLimits {
            ArtifactLimits {
                maximum_bytes: 32 * 1024 * 1024,
                open_deadline_ms: 30_000,
                chunk_deadline_ms: 10_000,
                chunk_bytes: 256 * 1024,
            }
        }
    }

    #[cfg(feature = "internal-capacity-probes")]
    #[path = "capacity_tests.rs"]
    mod capacity_tests;

    #[test]
    fn saved_artifact_identity_and_descriptor_guards_cover_pending_and_recheck() -> Result<()> {
        use crate::catalog_migration::{import_artifacts, importer};
        let (fixture, mut catalog) = RawFixture::new(17)?;
        let request = &fixture.requests[0];
        let mut reader =
            catalog.open_migration_artifact(request.clone(), RawFixture::limits(), &|| false)?;
        let before = catalog.begin_migration_artifact(&mut reader)?;
        let encoded = reader.encoded().to_vec();
        drop(reader);
        let source = fixture.inspection.open();
        let policy = importer::Policy {
            import_source: "saved-artifact-guards".into(),
            overlap: importer::OverlapPolicy::RequireDecision,
            keyword_overlap: importer::KeywordOverlap::RequireDecision,
            artifacts: vec![importer::ArtifactInput {
                capture_revision: fixture.inspection.revision().into(),
                member_index: request.member_index,
                mapping: request.mapping.clone(),
            }],
            supplements: vec![],
        };
        let progress = importer::Progress {
            id: "synthetic pending checkpoint".into(),
            input: source.binding_blake3().into(),
            stage: importer::Stage::ArtifactCustody,
            capture_index: 0,
            artifact_index: 0,
            cursor: None,
            processed: 0,
            complete: false,
        };
        for expression in [
            "replace(hex(zeroblob(524288)), '0', 'x')",
            "replace(hex(zeroblob(33)), '0', 'é')",
            "CAST(x'ff' || zeroblob(63) AS TEXT)",
        ] {
            catalog
                .db
                .execute_batch("SAVEPOINT malformed_identity; PRAGMA defer_foreign_keys=ON")?;
            catalog.db.execute(
                &format!("UPDATE migration_evidence SET id={expression} WHERE id=?1"),
                [&before.id],
            )?;
            catalog.db.execute(
                &format!("UPDATE migration_artifacts SET evidence={expression} WHERE evidence=?1"),
                [&before.id],
            )?;
            assert_eq!(
                catalog
                    .db
                    .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                        .get::<_, i64>(
                        0
                    ))?,
                0
            );
            for error in [
                existing(&catalog.db, &encoded, request).unwrap_err(),
                catalog
                    .migration_artifact(request.retained_capture_record, request.member_index)
                    .unwrap_err(),
                import_artifacts::pending(&mut catalog, &source, &progress, &policy).unwrap_err(),
            ] {
                assert!(
                    format!("{error:#}").contains("64 bytes of UTF-8 TEXT"),
                    "{error:#}"
                );
            }
            catalog
                .db
                .execute_batch("ROLLBACK TO malformed_identity; RELEASE malformed_identity")?;
            assert_eq!(
                catalog
                    .migration_artifact(request.retained_capture_record, request.member_index)?
                    .1,
                before
            );
        }
        catalog.db.execute(
            "UPDATE migration_evidence SET descriptor=zeroblob(65537) WHERE id=?1",
            [&before.id],
        )?;
        let error =
            import_artifacts::pending(&mut catalog, &source, &progress, &policy).unwrap_err();
        assert!(format!("{error:#}").contains("BLOB of at most 64 KiB"));
        catalog.db.execute(
            "UPDATE migration_evidence SET descriptor=?2 WHERE id=?1",
            params![before.id, encoded],
        )?;
        assert_eq!(
            import_artifacts::pending(&mut catalog, &source, &progress, &policy)?
                .progress
                .artifact_index,
            0
        );
        assert_eq!(
            catalog
                .migration_artifact(request.retained_capture_record, request.member_index)?
                .1,
            before
        );
        Ok(())
    }

    #[test]
    fn artifact_opening_retention_columns_reject_before_materialization() -> Result<()> {
        let (fixture, catalog) = RawFixture::new(17)?;
        let request = &fixture.requests[0];
        let original = serde_json::to_vec(&descriptor(&catalog.db, request)?)?;
        for (sql, expected) in [
            (
                "UPDATE migration_retention SET seal=zeroblob(8388609)",
                "retained seal type/size",
            ),
            (
                "UPDATE migration_retention SET seal='{}'",
                "retained seal type/size",
            ),
            (
                "UPDATE migration_retained_records SET compressed=zeroblob(8421377)",
                "retained record type/size",
            ),
            (
                "UPDATE migration_retained_records SET compressed='bad'",
                "retained record type/size",
            ),
            (
                "UPDATE migration_retained_records SET digest=replace(hex(zeroblob(33)),'0','é')",
                "retained digest type/size",
            ),
            (
                "UPDATE migration_retained_records SET raw_length=-1",
                "Integer",
            ),
        ] {
            catalog.db.execute_batch("SAVEPOINT corrupt")?;
            catalog.db.execute_batch(sql)?;
            let error = descriptor(&catalog.db, request).unwrap_err();
            if expected != "Integer" {
                assert!(format!("{error:#}").contains(expected), "{sql}: {error:#}");
            }
            catalog
                .db
                .execute_batch("ROLLBACK TO corrupt; RELEASE corrupt")?;
            assert_eq!(
                serde_json::to_vec(&descriptor(&catalog.db, request)?)?,
                original
            );
        }
        Ok(())
    }

    #[test]
    fn main_and_opaque_auxiliary_resume_and_remain_available_offline() -> Result<()> {
        let (fixture, mut catalog) = RawFixture::new(17 * 1024 * 1024 + 17)?;
        let mut ids = Vec::new();
        for request in &fixture.requests {
            let mut reader =
                catalog
                    .open_migration_artifact(request.clone(), RawFixture::limits(), &|| false)?;
            assert_ne!(
                reader.descriptor().artifact.revision.object,
                request.mapping.copy_identity.object
            );
            let started = catalog.begin_migration_artifact(&mut reader)?;
            let first = catalog.step_migration_artifact(&mut reader, &|| false)?;
            assert!(first.committed > 0);
            assert!(
                catalog
                    .step_migration_artifact(&mut reader, &|| true)
                    .is_err()
            );
            assert_eq!(catalog.migration_evidence(&first.id)?, first);
            drop(reader);
            drop(catalog);
            catalog = Catalog::open(&fixture.catalog)?;
            let mut reader =
                catalog
                    .open_migration_artifact(request.clone(), RawFixture::limits(), &|| false)?;
            assert_eq!(catalog.begin_migration_artifact(&mut reader)?, first);
            let mut done = first;
            while !done.complete {
                done = catalog.step_migration_artifact(&mut reader, &|| false)?;
            }
            assert_eq!(
                catalog.step_migration_artifact(&mut reader, &|| false)?,
                done
            );
            assert_eq!(started.id, done.id);
            assert_eq!(
                catalog
                    .migration_artifact(request.retained_capture_record, request.member_index)?
                    .1,
                done
            );
            ids.push(done.id);
        }
        for request in &fixture.requests {
            let path = mapping_path(&request.mapping)?;
            assert_eq!(fs::read(&path)?, fixture.expected[request.member_index]);
            fs::remove_file(path)?;
        }
        drop(catalog);
        let catalog = Catalog::open(&fixture.catalog)?;
        for (id, expected) in ids.iter().zip(&fixture.expected) {
            let mut hash = blake3::Hasher::new();
            let mut offset = 0;
            while offset < expected.len() as u64 {
                let bytes = catalog.migration_evidence_chunk(id, offset)?;
                hash.update(&bytes);
                offset += bytes.len() as u64;
            }
            assert_eq!(hash.finalize(), blake3::hash(expected));
            assert!(catalog.migration_evidence_chunk(id, offset)?.is_empty());
        }
        Ok(())
    }

    #[test]
    fn authority_member_manifest_mapping_and_exclusion_reject_before_custody() -> Result<()> {
        let (fixture, mut catalog) = RawFixture::new(4096)?;
        let base = fixture.requests[0].clone();
        let mut bad = base.clone();
        bad.member_index = 2;
        assert!(
            catalog
                .open_migration_artifact(bad, RawFixture::limits(), &|| false)
                .is_err()
        );
        let source = fixture.inspection.open();
        assert!(
            catalog
                .retained_migration_records(
                    source.binding_blake3(),
                    &fixture.inspection.seal.excluded_revisions[0],
                    Collection::Captures,
                    0,
                    10,
                )?
                .is_empty()
        );
        let rows = catalog.retained_migration_records(
            source.binding_blake3(),
            fixture.inspection.revision(),
            Collection::Rows,
            0,
            10,
        )?;
        let mut bad = base.clone();
        bad.retained_capture_record = rows[0].0;
        assert!(
            catalog
                .open_migration_artifact(bad, RawFixture::limits(), &|| false)
                .is_err()
        );
        for relative in [
            "../escape",
            "member-0.opaque/../member-1.opaque",
            "./member-0.opaque",
            "/absolute",
            "member-1.opaque",
        ] {
            let mut bad = base.clone();
            bad.mapping.relative = NativePath::from_path(Path::new(relative));
            assert!(
                catalog
                    .open_migration_artifact(bad, RawFixture::limits(), &|| false)
                    .is_err(),
                "{relative}"
            );
        }
        let mut bad = base.clone();
        #[cfg(unix)]
        {
            bad.mapping.relative = NativePath::WindowsWide(vec![97]);
        }
        #[cfg(windows)]
        {
            bad.mapping.relative = NativePath::UnixBytes(vec![97]);
        }
        assert!(
            catalog
                .open_migration_artifact(bad, RawFixture::limits(), &|| false)
                .is_err()
        );
        let mut reader =
            catalog.open_migration_artifact(base.clone(), RawFixture::limits(), &|| false)?;
        catalog.begin_migration_artifact(&mut reader)?;
        drop(reader);
        let alternate = fixture.requests[1].mapping.clone();
        let mut bad = base.clone();
        bad.mapping = alternate;
        assert!(
            catalog
                .open_migration_artifact(bad, RawFixture::limits(), &|| false)
                .is_err()
        );
        // Corrupting only a retained approval/manifest field is not authority.
        catalog.db.execute(
            "UPDATE migration_retention SET seal=replace(CAST(seal AS TEXT),?1,?2)",
            params![
                fixture.inspection.revision(),
                fixture.inspection.seal.excluded_revisions[0]
            ],
        )?;
        assert!(
            catalog
                .open_migration_artifact(base, RawFixture::limits(), &|| false)
                .is_err()
        );
        assert_eq!(
            catalog
                .db
                .query_row("SELECT count(*) FROM migration_artifacts", [], |r| r
                    .get::<_, i64>(0))?,
            1
        );
        Ok(())
    }

    #[test]
    fn changed_artifact_is_rejected_or_windows_writer_is_denied() -> Result<()> {
        use std::io::Write;
        let (fixture, mut catalog) = RawFixture::new(4096)?;
        let request = fixture.requests[0].clone();
        let path = mapping_path(&request.mapping)?;
        let mut reader =
            catalog.open_migration_artifact(request, RawFixture::limits(), &|| false)?;
        let state = catalog.begin_migration_artifact(&mut reader)?;
        match fs::OpenOptions::new().write(true).open(&path) {
            Ok(mut file) => {
                file.write_all(b"changed")?;
                file.sync_all()?;
                assert!(
                    catalog
                        .step_migration_artifact(&mut reader, &|| false)
                        .is_err()
                );
                assert_eq!(catalog.migration_evidence(&state.id)?, state);
            }
            Err(error) => {
                #[cfg(not(windows))]
                panic!("unexpected source refusal: {error}");
                #[cfg(windows)]
                {
                    assert_eq!(error.raw_os_error(), Some(32));
                    assert!(
                        catalog
                            .step_migration_artifact(&mut reader, &|| false)?
                            .complete
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn admission_stop_limits_wrong_digest_and_empty_artifact() -> Result<()> {
        let (fixture, mut catalog) = RawFixture::new(0)?;
        let request = fixture.requests[0].clone();
        assert!(
            catalog
                .open_migration_artifact(request.clone(), RawFixture::limits(), &|| true)
                .is_err()
        );
        let mut limits = RawFixture::limits();
        limits.chunk_bytes = evidence::CHUNK_BYTES + 1;
        assert!(
            catalog
                .open_migration_artifact(request.clone(), limits, &|| false)
                .is_err()
        );
        let mut reader =
            catalog.open_migration_artifact(request, RawFixture::limits(), &|| false)?;
        assert!(catalog.begin_migration_artifact(&mut reader)?.complete);
        let mut request = fixture.requests[1].clone();
        let path = mapping_path(&request.mapping)?;
        fs::write(&path, [1, 2, 3, 4, 5])?;
        request.mapping.copy_identity = Source::open(&path, 100)?.before.clone();
        assert!(
            catalog
                .open_migration_artifact(request, RawFixture::limits(), &|| false)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn owned_mapping_grants_precede_paths_and_keep_legacy_native_spelling() -> Result<()> {
        let (fixture, _catalog) = RawFixture::new(10)?;
        let mapping = &fixture.requests[0].mapping;
        let expected = mapping_path(mapping)?;
        let mut attempts = 0;
        let denied = admitted_mapping_path(mapping, &mut |_| {
            attempts += 1;
            anyhow::bail!("synthetic no allocation allowance")
        });
        assert!(
            denied
                .unwrap_err()
                .to_string()
                .contains("synthetic no allocation allowance")
        );
        assert_eq!(attempts, 1);
        let mut total = 0usize;
        let actual = admitted_mapping_path(mapping, &mut |bytes| {
            total = total.checked_add(bytes).unwrap();
            Ok(())
        })?;
        assert_eq!(actual, expected);
        assert!(total > actual.as_os_str().len());
        let mut foreign = mapping.clone();
        #[cfg(unix)]
        {
            foreign.relative = NativePath::WindowsWide(vec![0xd800]);
        }
        #[cfg(windows)]
        {
            foreign.relative = NativePath::UnixBytes(vec![255]);
        }
        assert!(admitted_mapping_path(&foreign, &mut |_| Ok(())).is_err());
        assert!(mapping_path(&foreign).is_err());
        println!("RAW_PATH admitted={total} original_native_spelling=true foreign_rejected=true");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlink_roots_and_members_are_not_followed() -> Result<()> {
        let (fixture, catalog) = RawFixture::new(10)?;
        let mut request = fixture.requests[0].clone();
        let path = mapping_path(&request.mapping)?;
        let alias = path.with_file_name("linked");
        std::os::unix::fs::symlink(&path, &alias)?;
        request.mapping.relative = NativePath::from_path(Path::new("linked"));
        assert!(
            catalog
                .open_migration_artifact(request, RawFixture::limits(), &|| false)
                .is_err()
        );
        let mut request = fixture.requests[0].clone();
        let root = request.mapping.root.to_path()?;
        let alias = root.with_file_name("linked-root");
        std::os::unix::fs::symlink(&root, &alias)?;
        request.mapping.root = NativePath::from_path(&alias);
        assert!(
            catalog
                .open_migration_artifact(request, RawFixture::limits(), &|| false)
                .is_err()
        );
        Ok(())
    }
}
