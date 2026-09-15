use super::*;
use crate::catalog_migration::{
    file_metadata::Origin,
    importer::{Policy, SupplementInput},
};
use anyhow::{Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

impl SelectionReview {
    /// Builds canonical authority documents from a typed, explicitly reviewed
    /// draft while the exact selection snapshot is still pinned. Artifact copy
    /// identities and supplement evidence are supplied by their isolated owners;
    /// this factory validates roster coverage and never opens their paths.
    pub fn approval_documents(
        &self,
        bytes: &[u8],
        cancel: Arc<AtomicBool>,
    ) -> Result<ApprovalDocuments> {
        let limits = self.summary.limits;
        ensure!(
            !bytes.is_empty()
                && bytes.len() <= MANIFEST_BYTES
                && bytes.len() <= limits.review_bytes,
            "approval draft byte admission exceeded"
        );
        let draft: ApprovalDraft = serde_json::from_slice(bytes)?;
        ensure!(
            draft.protocol == 1 && draft.review_token == self.summary.token,
            "approval draft protocol or review token differs"
        );
        self.current(&draft.review_token)?;
        check(
            &cancel,
            Instant::now() + Duration::from_millis(limits.deadline_ms),
        )?;
        local(&draft.destination, limits.native_path_units)?;
        ensure!(
            !draft.import_source.trim().is_empty()
                && draft.import_source.len() <= 4096
                && !draft.import_source.contains('\0'),
            "approval import source bounds"
        );
        ensure!(
            !draft.authorization.trim().is_empty()
                && draft.authorization.len() <= 4096
                && !draft.authorization.contains('\0'),
            "explicit bounded authorization required"
        );
        ensure!(
            draft.artifacts.len() <= 16_384 && draft.supplements.len() <= 16_384,
            "approval roster bound"
        );
        let selected: BTreeSet<_> = self
            .evidence
            .captures
            .iter()
            .filter(|v| v.selected)
            .map(|v| v.revision.as_str())
            .collect();
        let artifacts = draft
            .artifacts
            .iter()
            .map(|raw| {
                ensure!(
                    raw.json.len() <= 65_536
                        && blake3::hash(raw.json.as_bytes()).to_hex().as_str() == raw.blake3,
                    "prepared artifact document digest differs"
                );
                Ok(serde_json::from_str::<
                    crate::catalog_migration::importer::ArtifactInput,
                >(&raw.json)?)
            })
            .collect::<Result<Vec<_>>>()?;
        let supplements = draft
            .supplements
            .iter()
            .map(|raw| {
                ensure!(
                    raw.json.len() <= 8 * 1024 * 1024
                        && blake3::hash(raw.json.as_bytes()).to_hex().as_str() == raw.blake3,
                    "prepared supplement document digest differs"
                );
                Ok(serde_json::from_str::<
                    crate::catalog_migration::supplements::Prepared,
                >(&raw.json)?)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut required = BTreeMap::new();
        for revision in &selected {
            let manifest_json: String = self.plan.db.query_row(
                "SELECT manifest FROM captures WHERE revision=?1",
                [revision],
                |r| r.get(0),
            )?;
            let manifest: crate::lightroom::capture::Manifest =
                serde_json::from_str(&manifest_json)?;
            ensure!(
                manifest.revision_id.as_deref() == Some(*revision)
                    && crate::lightroom::json_digest(&manifest.artifacts)? == *revision,
                "selected artifact manifest changed"
            );
            for (index, artifact) in manifest.artifacts.iter().enumerate() {
                ensure!(
                    required
                        .insert(
                            ((*revision).to_string(), index),
                            NativePath::from_path(std::path::Path::new(&artifact.stored)),
                        )
                        .is_none(),
                    "duplicate selected artifact member"
                );
            }
        }
        let mut artifact_keys = BTreeSet::new();
        for artifact in &artifacts {
            ensure!(
                selected.contains(artifact.capture_revision.as_str()),
                "approval artifact is outside selected roster"
            );
            ensure!(
                artifact_keys.insert((artifact.capture_revision.as_str(), artifact.member_index)),
                "duplicate approval artifact member"
            );
            ensure!(
                required.get(&(artifact.capture_revision.clone(), artifact.member_index,))
                    == Some(&artifact.mapping.relative),
                "prepared artifact relative path differs from selected manifest"
            );
            native(&artifact.mapping.root, limits.native_path_units)?;
            native(&artifact.mapping.relative, limits.native_path_units)?;
        }
        ensure!(
            artifact_keys.len() == required.len()
                && required
                    .keys()
                    .all(|(revision, index)| artifact_keys.contains(&(revision.as_str(), *index))),
            "approval artifact roster does not cover every selected manifest member"
        );
        let mut supplement_keys = BTreeSet::new();
        for prepared in &supplements {
            let pin = &prepared.pin;
            ensure!(
                selected.contains(pin.revision.as_str())
                    && pin.origin == "embedded"
                    && supplement_keys.insert((pin.revision.as_str(), pin.source_id.as_str())),
                "approval supplement is outside selected roster or duplicated"
            );
            ensure!(
                !prepared.evidence.is_empty() && prepared.evidence.len() <= 128,
                "supplement evidence ID bounds"
            );
            let evidence: Vec<u8> = self.plan.db.query_row(
                "SELECT CAST(evidence AS BLOB) FROM paths WHERE revision=?1 AND source_id=?2 AND evidence IS NOT NULL",
                rusqlite::params![pin.revision, pin.source_id],
                |r| r.get(0),
            )?;
            ensure!(
                evidence.len() <= crate::lightroom::PAGE_BYTES,
                "approval supplement source evidence bound"
            );
            crate::lightroom::migration_source::validate_supplement_pin(&evidence, pin, &|| {
                cancel.load(Ordering::Relaxed)
            })?;
        }
        let policy = Policy {
            import_source: draft.import_source,
            overlap: draft.overlap,
            keyword_overlap: draft.keyword_overlap,
            artifacts,
            supplements: supplements
                .iter()
                .map(|v| SupplementInput {
                    capture_revision: v.pin.revision.clone(),
                    source_id: v.pin.source_id.clone(),
                    origin: Origin::Embedded,
                    evidence: v.evidence.clone(),
                })
                .collect(),
        };
        let approval = ApprovalDocument {
            protocol: 1,
            review_token: draft.review_token,
            scope: ApprovalScope::SelectedMigration,
            destination: draft.destination,
            policy: policy.clone(),
            supplements: supplements.into_iter().map(|v| v.pin).collect(),
            authorization: draft.authorization,
        };
        let approval_bytes = bounded_json(&approval, limits.review_bytes)?;
        let policy_bytes = bounded_json(&policy, limits.review_bytes)?;
        self.current(&approval.review_token)?;
        Ok(ApprovalDocuments {
            approval_blake3: blake3::hash(&approval_bytes).to_hex().to_string(),
            approval_json: String::from_utf8(approval_bytes)?,
            policy_blake3: blake3::hash(&policy_bytes).to_hex().to_string(),
            policy_json: String::from_utf8(policy_bytes)?,
        })
    }
}
