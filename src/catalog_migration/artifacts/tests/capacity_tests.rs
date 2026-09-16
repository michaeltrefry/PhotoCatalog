use super::*;
use crate::capacity_probes as probe;

#[test]
fn capacity_parent_descriptor_measures_success_and_rejection_clones() -> Result<()> {
    let baseline = probe::begin();
    for external in [false, true] {
        let (fixture, catalog) = RawFixture::with_manifest(1024, |manifest| {
            if external {
                manifest.issues.push(crate::lightroom::Issue {
                    code: "preserved".into(),
                    source_id: None,
                    detail: "x".repeat(1024 * 1024),
                });
                // Three decimal digits plus each separator put this near64KiB.
                manifest.artifacts[0].source = NativePath::UnixBytes(vec![120; 14_000]);
            }
        })?;
        let request = &fixture.requests[0];
        let record = retention::selected_capture(&catalog.db, request.retained_capture_record)?;
        assert_eq!(
            matches!(
                record.fields["manifest"],
                crate::lightroom::migration_source::Field::Bytes(_)
            ),
            external
        );
        let before = catalog.db.total_changes();
        let result = descriptor(&catalog.db, request)?;
        let encoded = crate::lightroom::bounded_json(&result, DESCRIPTOR_LIMIT)?;
        assert!(encoded.len() <= DESCRIPTOR_LIMIT);
        if external {
            assert!(encoded.len() >= 50_000);
        }
        let mut oversized_request = request.clone();
        oversized_request.mapping.relative = NativePath::UnixBytes(vec![120; 100_000]);
        let error = descriptor(&catalog.db, &oversized_request).unwrap_err();
        assert!(format!("{error:#}").contains("limit"));
        assert_eq!(catalog.db.total_changes(), before);
        println!(
            "CAPACITY_DESCRIPTOR external={external} accepted_bytes={} descriptor_inline={} chosen_owned={} rejected_request_units={}",
            encoded.len(),
            std::mem::size_of::<ArtifactDescriptor>(),
            probe::artifact(&result.artifact),
            100_000
        );
    }
    let (fixture, catalog) = RawFixture::with_manifest(1024, |manifest| {
        manifest.artifacts[0].source = NativePath::UnixBytes(vec![97; 40_000]);
    })?;
    let error = descriptor(&catalog.db, &fixture.requests[0]).unwrap_err();
    assert!(format!("{error:#}").contains("limit"));
    assert!(probe::capacity(probe::DESCRIPTOR_CLONES) >= 100_000);
    for event in [
        probe::DESCRIPTOR_RECORD,
        probe::DESCRIPTOR_BYTES,
        probe::DESCRIPTOR_MANIFEST,
        probe::DESCRIPTOR_SEAL,
        probe::DESCRIPTOR_CLONES,
    ] {
        assert!(probe::visits(event) > 0);
    }
    probe::report("parent-descriptor", baseline);
    Ok(())
}

#[test]
fn capacity_custody_consumer_keeps_two_manifests_and_one_raw_reader() -> Result<()> {
    use crate::catalog_migration::{import_artifacts, importer};
    let baseline = probe::begin();
    let (fixture, mut catalog) = RawFixture::with_manifest(600_000, |manifest| {
        manifest.issues.push(crate::lightroom::Issue {
            code: "observed".into(),
            source_id: None,
            detail: "x".repeat(256 * 1024),
        });
    })?;
    let source = fixture.inspection.open();
    let policy = importer::Policy {
        import_source: "capacity-consumer".into(),
        overlap: importer::OverlapPolicy::RequireDecision,
        keyword_overlap: importer::KeywordOverlap::RequireDecision,
        artifacts: fixture
            .requests
            .iter()
            .map(|r| importer::ArtifactInput {
                capture_revision: fixture.inspection.revision().into(),
                member_index: r.member_index,
                mapping: r.mapping.clone(),
            })
            .collect(),
        supplements: vec![],
    };
    let approval = b"synthetic selected raw custody approval";
    let progress = catalog.begin_selected_import(&source, approval, &policy)?;
    // Explicit synthetic saved checkpoint targets the existing custody engine;
    // prior retention is real, complete and source-bound in RawFixture.
    let mut cursor = progress.clone();
    cursor.stage = importer::Stage::ArtifactCustody;
    catalog.db.execute(
        "UPDATE migration_runs SET progress=?2 WHERE id=?1",
        params![progress.id, serde_json::to_vec(&cursor)?],
    )?;
    let mut worker = import_artifacts::Worker::new(&source, &progress.id, RawFixture::limits())?;
    for _ in 0..3 {
        worker.step(&mut catalog, &|| false)?;
    }
    assert!(probe::visits(probe::PENDING_MANIFEST) >= 3);
    assert!(probe::visits(probe::REQUEST_MANIFEST) >= 3);
    assert!(probe::visits(probe::TWO_MANIFESTS) >= 3);
    assert!(probe::capacity(probe::PENDING_MANIFEST) >= 256 * 1024);
    assert!(probe::capacity(probe::REQUEST_MANIFEST) >= 256 * 1024);
    assert!(probe::visits(probe::CHUNK_VERIFY) > 0);
    drop(worker);
    drop(source);
    probe::report("custody-consumer", baseline);
    Ok(())
}
