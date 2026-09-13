//! The import worker owns one raw-capture reader at a time. Source admission and
//! compression happen outside destination writer transactions.
use super::{
    artifacts::{ArtifactLimits, ArtifactReader, ArtifactRequest},
    importer::{self, Policy, Progress, Stage, Step},
};
use crate::{
    Catalog,
    lightroom::migration_source::{Collection, MigrationSource},
};
use anyhow::{Context, Result, ensure};
use rusqlite::OptionalExtension;

fn request(
    catalog: &Catalog,
    source: &MigrationSource,
    progress: &Progress,
    policy: &Policy,
) -> Result<Option<ArtifactRequest>> {
    let Some(capture) = source.seal().selected.get(progress.capture_index) else {
        return Ok(None);
    };
    let manifest = source.capture_manifest(&capture.revision)?;
    if progress.artifact_index >= manifest.artifacts.len() {
        return Ok(None);
    }
    let rows = catalog.retained_migration_records(
        &progress.input,
        &capture.revision,
        Collection::Captures,
        0,
        2,
    )?;
    ensure!(
        rows.len() == 1,
        "raw artifact requires one selected capture record"
    );
    let mapping = policy
        .artifacts
        .iter()
        .find(|a| {
            a.capture_revision == capture.revision && a.member_index == progress.artifact_index
        })
        .context("raw artifact requires an explicit sealed copy mapping")?;
    Ok(Some(ArtifactRequest {
        retained_capture_record: rows[0].0,
        member_index: progress.artifact_index,
        mapping: mapping.mapping.clone(),
    }))
}
pub(crate) fn pending(
    catalog: &mut Catalog,
    source: &MigrationSource,
    before: &Progress,
    policy: &Policy,
) -> Result<Step> {
    let mut after = before.clone();
    let Some(capture) = source.seal().selected.get(before.capture_index) else {
        ensure!(
            before.capture_index == source.seal().selected.len(),
            "artifact capture cursor bounds"
        );
        after.stage = Stage::Reconciliation;
        after.capture_index = 0;
        after.artifact_index = 0;
        importer::advance(catalog, before, &after, None)?;
        return Ok(Step {
            progress: after,
            outcome: None,
            needs_decision: None,
        });
    };
    let manifest = source.capture_manifest(&capture.revision)?;
    if before.artifact_index == manifest.artifacts.len() {
        after.capture_index += 1;
        after.artifact_index = 0;
        importer::advance(catalog, before, &after, None)?;
        return Ok(Step {
            progress: after,
            outcome: None,
            needs_decision: None,
        });
    }
    ensure!(
        before.artifact_index < manifest.artifacts.len(),
        "artifact member cursor bounds"
    );
    if !policy
        .artifacts
        .iter()
        .any(|a| a.capture_revision == capture.revision && a.member_index == before.artifact_index)
    {
        return Ok(Step {
            progress: before.clone(),
            outcome: None,
            needs_decision: Some(format!(
                "Captured artifact {} member {} needs an explicit sealed copy mapping",
                capture.revision, before.artifact_index
            )),
        });
    }
    let request = request(catalog, source, before, policy)?.context("artifact request absent")?;
    let found:Option<String>=catalog.db.query_row("SELECT evidence FROM migration_artifacts WHERE retained_capture_record=?1 AND member_index=?2",rusqlite::params![request.retained_capture_record,i64::try_from(request.member_index)?],|r|r.get(0)).optional()?;
    if found.is_some() {
        let (descriptor, state) =
            catalog.migration_artifact(request.retained_capture_record, request.member_index)?;
        ensure!(
            descriptor.selected_input == before.input
                && descriptor.capture_revision == capture.revision
                && descriptor.manifest_blake3 == capture.manifest_blake3
                && serde_json::to_vec(&descriptor.request)? == serde_json::to_vec(&request)?
                && serde_json::to_vec(&descriptor.artifact)?
                    == serde_json::to_vec(&manifest.artifacts[before.artifact_index])?,
            "raw artifact source or copy mapping differs"
        );
        if state.complete {
            after.artifact_index += 1;
            importer::advance(catalog, before, &after, None)?;
        }
    }
    Ok(Step {
        progress: after,
        outcome: None,
        needs_decision: None,
    })
}
/// Keep this worker alive across steps to hash each captured file once on open.
/// Dropping it releases source handles; reopening resumes its durable chunks.
pub struct Worker<'a> {
    source: &'a MigrationSource,
    run: String,
    limits: ArtifactLimits,
    artifact: Option<ArtifactReader>,
}
impl<'a> Worker<'a> {
    pub fn new(source: &'a MigrationSource, run: &str, limits: ArtifactLimits) -> Result<Self> {
        ensure!(
            run.len() == 64 && run.bytes().all(|v| v.is_ascii_hexdigit()),
            "migration run identity bounds"
        );
        Ok(Self {
            source,
            run: run.into(),
            limits,
            artifact: None,
        })
    }
    pub fn step(&mut self, catalog: &mut Catalog, stop: &dyn Fn() -> bool) -> Result<Step> {
        ensure!(!stop(), "migration stopped before step");
        let before = catalog.selected_import_progress(&self.run)?;
        let step = catalog.step_selected_import(self.source, &self.run)?;
        if before.stage != Stage::ArtifactCustody
            || step.progress.stage != Stage::ArtifactCustody
            || step.needs_decision.is_some()
            || before.capture_index != step.progress.capture_index
            || before.artifact_index != step.progress.artifact_index
        {
            self.artifact = None;
            return Ok(step);
        }
        let (_, policy) = importer::read(&catalog.db, &self.run)?;
        let request = request(catalog, self.source, &step.progress, &policy)?
            .context("artifact stage has no request")?;
        if let Some(reader) = &self.artifact {
            ensure!(
                serde_json::to_vec(&reader.descriptor().request)? == serde_json::to_vec(&request)?,
                "worker artifact cursor changed"
            );
        } else {
            self.artifact = Some(catalog.open_migration_artifact(request, self.limits, stop)?);
        }
        let reader = self.artifact.as_mut().unwrap();
        catalog.begin_migration_artifact(reader)?;
        let state = catalog.step_migration_artifact(reader, stop)?;
        if state.complete {
            self.artifact = None;
            return catalog.step_selected_import(self.source, &self.run);
        }
        Ok(step)
    }
}
