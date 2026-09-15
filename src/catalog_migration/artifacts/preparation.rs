//! Pre-seal custody preparation uses exact reviewed manifest bytes plus an
//! explicitly selected retained copy. Historical source locators stay opaque.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingPreparation {
    pub capture_revision: String,
    pub manifest_blake3: String,
    pub member_index: usize,
    pub root: NativePath,
    pub relative: NativePath,
}

/// Prepare one member from an already parsed and digest-verified capture
/// manifest. The caller owns the immutable manifest snapshot and supplies the
/// relative path taken from that exact snapshot.
pub fn prepare_manifest_artifact(
    artifact: &Artifact,
    root: &NativePath,
    relative: &NativePath,
    limits: ArtifactLimits,
    stop: &dyn Fn() -> bool,
) -> Result<ArtifactMapping> {
    limits.validate()?;
    ensure!(
        artifact.revision.bytes <= limits.maximum_bytes,
        "artifact exceeds declared maximum bytes"
    );
    ensure!(!stop(), "artifact preparation stopped");
    let deadline = Instant::now() + Duration::from_millis(limits.open_deadline_ms);
    let path = mapping_path_parts(root, relative)?;
    #[cfg(windows)]
    let _write_lease = {
        use std::os::windows::fs::OpenOptionsExt;
        reject_links(&path)?;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)?
    };
    let mut source = Source::open(&path, limits.maximum_bytes)?;
    ensure!(
        source.before.bytes == artifact.revision.bytes,
        "artifact retained copy length differs"
    );
    source.lock(0, 0)?;
    let mut hash = blake3::Hasher::new();
    let mut remaining = source.before.bytes;
    let mut buffer = [0; 128 * 1024];
    while remaining != 0 {
        ensure!(
            Instant::now() < deadline && !stop(),
            "artifact preparation deadline/stop"
        );
        let size = remaining.min(buffer.len() as u64) as usize;
        source.file.read_exact(&mut buffer[..size])?;
        hash.update(&buffer[..size]);
        remaining -= size as u64;
    }
    source.verify()?;
    ensure!(
        hash.finalize().to_hex().as_str() == artifact.blake3,
        "artifact retained copy bytes differ from reviewed manifest"
    );
    ensure!(
        Instant::now() < deadline && !stop(),
        "artifact preparation deadline/stop"
    );
    Ok(ArtifactMapping {
        root: root.clone(),
        relative: relative.clone(),
        copy_identity: source.before.clone(),
    })
}
/// The caller must obtain `manifest_json` and its pinned revision/digest from
/// the live SelectionReview owner, or another already-validated sealed source.
/// This verifies bytes and the current retained-copy identity; it neither
/// approves a selection nor writes generic/destination custody.
///
/// Run in the dedicated migration helper: Source locks are process-scoped on
/// Unix. Cancellation is checked each 128KiB; the owner can terminate/reap a
/// helper whose filesystem read cannot be interrupted cooperatively.
pub fn prepare_mapping(
    manifest_json: &[u8],
    request: &MappingPreparation,
    limits: ArtifactLimits,
    stop: &dyn Fn() -> bool,
) -> Result<ArtifactMapping> {
    limits.validate()?;
    ensure!(!stop(), "artifact preparation stopped");
    ensure!(
        manifest_json.len() <= crate::lightroom::MANIFEST_BYTES,
        "artifact manifest byte admission"
    );
    ensure!(
        blake3::hash(manifest_json).to_hex().as_str() == request.manifest_blake3,
        "artifact reviewed manifest digest differs"
    );
    let manifest: Manifest = serde_json::from_slice(manifest_json)?;
    ensure!(
        manifest.protocol == 1
            && manifest.state == "captured"
            && manifest.revision_id.as_deref() == Some(request.capture_revision.as_str())
            && crate::lightroom::json_digest(&manifest.artifacts)? == request.capture_revision,
        "artifact reviewed capture revision differs"
    );
    let artifact = manifest
        .artifacts
        .get(request.member_index)
        .context("artifact reviewed member absent")?;
    ensure!(
        artifact.blake3.len() == 64
            && artifact
                .blake3
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "artifact manifest digest encoding"
    );
    prepare_manifest_artifact(artifact, &request.root, &request.relative, limits, stop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lightroom::migration_source::tests::Fixture;
    use std::path::Path;
    #[test]
    fn preparation_binds_current_copy_and_never_uses_foreign_original_locator() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let path = root.join("retained-copy");
        let bytes = b"exact retained raw artifact\0\xff";
        fs::write(&path, bytes)?;
        let current = Source::open(&path, 1024)?.before.clone();
        let fixture = Fixture::new();
        let source = fixture.open();
        let mut manifest = source.capture_manifest(fixture.revision())?;
        drop(source);
        let mut historical = current.clone();
        historical.object = "historical-original-object".into();
        historical.changed = "historical-original-change".into();
        let foreign = if cfg!(windows) {
            NativePath::UnixBytes(b"/never/open/original".to_vec())
        } else {
            NativePath::WindowsWide(vec![67, 58, 92, 0xd800])
        };
        manifest.artifacts = vec![Artifact {
            source: foreign,
            role: "main".into(),
            relative: NativePath::from_path(Path::new("old-name")),
            stored: "raw/old-name".into(),
            revision: historical.clone(),
            blake3: blake3::hash(bytes).to_hex().to_string(),
        }];
        let revision = crate::lightroom::json_digest(&manifest.artifacts)?;
        manifest.revision_id = Some(revision.clone());
        let raw = serde_json::to_vec(&manifest)?;
        let request = MappingPreparation {
            capture_revision: revision,
            manifest_blake3: blake3::hash(&raw).to_hex().to_string(),
            member_index: 0,
            root: NativePath::from_path(&root),
            relative: NativePath::from_path(Path::new("retained-copy")),
        };
        let limits = ArtifactLimits {
            maximum_bytes: 1024,
            open_deadline_ms: 1000,
            chunk_deadline_ms: 1000,
            chunk_bytes: 1024,
        };
        let mapped = prepare_mapping(&raw, &request, limits, &|| false)?;
        assert_eq!(mapped.copy_identity, current);
        assert_ne!(mapped.copy_identity, historical);
        assert_eq!(fs::read(&path)?, bytes);
        let mut stale = request.clone();
        stale.member_index = 1;
        assert!(prepare_mapping(&raw, &stale, limits, &|| false).is_err());
        let mut changed = raw.clone();
        changed.push(b' ');
        assert!(prepare_mapping(&changed, &request, limits, &|| false).is_err());
        assert!(prepare_mapping(&raw, &request, limits, &|| true).is_err());
        fs::write(&path, b"wrong retained raw artifact")?;
        assert!(prepare_mapping(&raw, &request, limits, &|| false).is_err());
        Ok(())
    }
}
