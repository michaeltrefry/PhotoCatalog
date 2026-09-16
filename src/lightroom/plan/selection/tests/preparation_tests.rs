use super::*;
#[test]
fn preparation_chunks_and_source_roster_remain_bound_to_selected_review() -> Result<()> {
    let case = Case::new();
    let review = case.review();
    let token = review.summary().token.clone();
    let revision = case.fixture.revision().to_owned();
    let page = review.preparation_sources(&token, &revision, 0, 1, flag())?;
    assert_eq!(page.sources.len(), 1);
    assert_eq!(page.sources[0].source_id, "file-selected");
    assert!(page.next.is_none());
    let evidence = review.preparation_chunk(
        &token,
        PreparationDocument::OriginalEvidence {
            revision: revision.clone(),
            source_id: page.sources[0].source_id.clone(),
        },
        0,
        65536,
        flag(),
    )?;
    assert_eq!(evidence.bytes, b"{\"missing\":true}");
    assert_eq!(evidence.state, "missing");
    let document = PreparationDocument::Manifest {
        revision: revision.clone(),
    };
    let mut offset = 0;
    let mut bytes = Vec::new();
    loop {
        let chunk = review.preparation_chunk(&token, document.clone(), offset, 7, flag())?;
        assert_eq!(chunk.offset, offset);
        assert_eq!(chunk.capture.revision, revision);
        bytes.extend(chunk.bytes);
        if let Some(next) = chunk.next {
            offset = next;
        } else {
            break;
        }
    }
    assert_eq!(
        blake3::hash(&bytes).to_hex().as_str(),
        page.capture.manifest_blake3
    );
    assert!(
        review
            .preparation_chunk("stale", document.clone(), 0, 16, flag())
            .is_err()
    );
    assert!(
        review
            .preparation_chunk(&token, document.clone(), 0, 65537, flag())
            .is_err()
    );
    assert!(
        review
            .preparation_chunk(&token, document, 0, 16, Arc::new(AtomicBool::new(true)))
            .is_err()
    );
    let excluded = case.fixture.seal.excluded_revisions[0].clone();
    assert!(
        review
            .preparation_sources(&token, &excluded, 0, 1, flag())
            .is_err()
    );
    assert!(
        review
            .preparation_chunk(
                &token,
                PreparationDocument::Manifest { revision: excluded },
                0,
                16,
                flag()
            )
            .is_err()
    );
    // Exact snapshots are consumed without resolving the deliberately absent
    // capture path installed by Case::new or its original provenance paths.
    assert!(
        !case
            .fixture
            .path
            .with_file_name("never-open-capture")
            .exists()
    );
    Ok(())
}
