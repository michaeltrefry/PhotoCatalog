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
    fn validate(self) -> Result<()> {
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
    for native in [&mapping.root, &mapping.relative] {
        let units = match native {
            NativePath::UnixBytes(v) => v.len(),
            NativePath::WindowsWide(v) => v.len(),
        };
        ensure!((1..=32768).contains(&units), "artifact mapping path length");
    }
    let root = mapping.root.to_path()?;
    let relative = mapping.relative.to_path()?;
    ensure!(
        root.is_absolute() && !relative.is_absolute(),
        "artifact mapping root/relative shape"
    );
    ensure!(
        relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
            && relative.components().count() <= 256,
        "artifact mapping contains non-normal component"
    );
    // components() normalizes a/./b; reject lexical dot components as well.
    match &mapping.relative {
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
    reject_links(&root)?;
    ensure!(
        fs::symlink_metadata(&root)?.is_dir(),
        "artifact sealed root is not a directory"
    );
    Ok(root.join(relative))
}

fn descriptor(db: &Connection, request: &ArtifactRequest) -> Result<ArtifactDescriptor> {
    ensure!(
        request.retained_capture_record > 0 && i64::try_from(request.member_index).is_ok(),
        "artifact member selector bounds"
    );
    let record = retention::selected_record(db, request.retained_capture_record)?;
    ensure!(
        record.collection == Collection::Captures,
        "artifact authority must be a retained Captures record"
    );
    let bytes = retention::field_bytes(
        db,
        request.retained_capture_record,
        &record,
        "manifest",
        crate::lightroom::MANIFEST_BYTES,
    )?;
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest.protocol == 1
            && manifest.state == "captured"
            && manifest.revision_id.as_deref() == Some(record.revision.as_str())
            && crate::lightroom::json_digest(&manifest.artifacts)? == record.revision,
        "retained artifact manifest revision differs"
    );
    let (input, seal): (String, Vec<u8>) = db.query_row("SELECT i.id,i.seal FROM migration_retained_records r JOIN migration_retention i ON i.id=r.input WHERE r.sequence=?1", [request.retained_capture_record], |r| Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(
        seal.len() <= crate::lightroom::MANIFEST_BYTES,
        "retained seal limit"
    );
    let seal: crate::lightroom::migration_source::InputSeal = serde_json::from_slice(&seal)?;
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
    crate::lightroom::bounded_json(&result, DESCRIPTOR_LIMIT)?;
    Ok(result)
}

fn existing(
    db: &Connection,
    descriptor: &[u8],
    request: &ArtifactRequest,
) -> Result<Option<String>> {
    let previous: Option<(Vec<u8>,String)> = db.query_row("SELECT descriptor,evidence FROM migration_artifacts WHERE retained_capture_record=?1 AND member_index=?2", params![request.retained_capture_record,i64::try_from(request.member_index)?], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((old, id)) = previous {
        ensure!(
            old == descriptor,
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
        let deadline = Instant::now() + Duration::from_millis(limits.open_deadline_ms);
        let descriptor = descriptor(&self.db, &request)?;
        let encoded = crate::lightroom::bounded_json(&descriptor, DESCRIPTOR_LIMIT)?;
        existing(&self.db, &encoded, &request)?;
        ensure!(
            descriptor.artifact.revision.bytes <= limits.maximum_bytes,
            "artifact exceeds declared maximum bytes"
        );
        let path = mapping_path(&request.mapping)?;
        #[cfg(windows)]
        let lease = {
            use std::os::windows::fs::OpenOptionsExt;
            reject_links(&path)?;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(&path)?
        };
        let mut source = Source::open(&path, limits.maximum_bytes)?;
        ensure!(
            source.before == request.mapping.copy_identity,
            "artifact sealed copy identity differs"
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

    pub fn begin_migration_artifact(
        &mut self,
        reader: &mut ArtifactReader,
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
                &descriptor(&tx, &reader.descriptor.request)?,
                DESCRIPTOR_LIMIT
            )? == reader.encoded,
            "artifact destination authority differs"
        );
        existing(&tx, &reader.encoded, &reader.descriptor.request)?;
        reader.verify()?;
        let result = evidence::begin_owned(
            &tx,
            &reader.encoded,
            reader.source.before.bytes,
            evidence::Authority::CapturedArtifact,
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO migration_artifacts VALUES(?1,?2,?3,?4)",
            params![
                reader.descriptor.request.retained_capture_record,
                i64::try_from(reader.descriptor.request.member_index)?,
                reader.encoded,
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
        reader.verify()?;
        ensure!(!stop(), "artifact custody stopped");
        let id = existing(&self.db, &reader.encoded, &reader.descriptor.request)?
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
            existing(&tx, &reader.encoded, &reader.descriptor.request)?.as_deref()
                == Some(id.as_str()),
            "artifact custody changed"
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
        let (bytes,id):(Vec<u8>,String)=self.db.query_row("SELECT descriptor,evidence FROM migration_artifacts WHERE retained_capture_record=?1 AND member_index=?2",params![retained_capture_record,i64::try_from(member_index)?],|r|Ok((r.get(0)?,r.get(1)?)))?;
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
