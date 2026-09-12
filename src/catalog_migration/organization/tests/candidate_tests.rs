use super::*;
use crate::{
    catalog_metadata::Source,
    xmp::{self, Edit, Value},
};

fn observe(
    b: &mut Bed,
    key: &VariantKey,
    name: &str,
    ambiguous: bool,
    edits: &[Edit],
) -> Result<()> {
    let id = b.catalog.image_metadata_identity(key)?.image_id;
    let bytes = xmp::apply_organization_edits(&xmp::empty_packet()?, edits)?;
    b.catalog.retain_image_metadata_id(
        &id,
        &Source {
            kind: "sidecar".into(),
            locator: name.as_bytes().to_vec(),
            display: name.into(),
            ambiguous,
            provenance: serde_json::json!({"synthetic":true}),
        },
        &candidates::inspection(bytes, false),
        false,
    )?;
    Ok(())
}
fn set(path: &str, value: &str) -> Edit {
    Edit::Set {
        namespace: xmp::XMP.into(),
        path: path.into(),
        value: value.into(),
    }
}
fn choice_count(b: &Bed, key: &VariantKey) -> Result<i64> {
    let id = b.catalog.image_metadata_identity(key)?.image_id;
    Ok(b.catalog.db.query_row(
        "SELECT count(*) FROM metadata_choices WHERE asset_id=?",
        [id],
        |r| r.get(0),
    )?)
}
fn member(b: &Bed, row: i64, image: i64, term: i64) -> Projection {
    b.request(
        row,
        Decision::KeywordMembership {
            image: b.link(row, "image", image),
            keyword: b.link(row, "tag", term),
        },
    )
}
fn model(target: &NativeTarget) -> i64 {
    match target {
        NativeTarget::MetadataCandidate { model_id, .. } => *model_id,
        _ => panic!("expected candidate: {target:?}"),
    }
}

#[test]
fn equal_ambiguous_rating_is_retained_without_precedence_and_partial_row_replays() -> Result<()> {
    let mut b = Bed::new(false)?;
    let (key, sibling) = b.images()?;
    let flag = b.request(300, Decision::Flag { value: Flag::Pick });
    let label = b.request(
        300,
        Decision::Label {
            value: "Red".into(),
        },
    );
    let first_flag = b
        .catalog
        .project_migration_organization(Some(&b.source), &flag)?;
    let first_label = b
        .catalog
        .project_migration_organization(Some(&b.source), &label)?;
    observe(&mut b, &key, "uncertain-a", true, &[set("Rating", "0")])?;
    observe(&mut b, &key, "uncertain-b", true, &[set("Rating", "0")])?;
    let before = b.catalog.image_metadata_identity(&key)?;
    assert!(
        b.catalog
            .organize_image(
                &key,
                before.metadata_revision,
                Operation::Rating { value: 0 }
            )
            .is_err()
    );
    let choices = choice_count(&b, &key)?;
    let sibling_before = b.catalog.image_metadata_identity(&sibling)?;
    let request = b.request(300, Decision::Rating { value: 0 });
    let result = b
        .catalog
        .project_migration_organization(Some(&b.source), &request)?;
    assert!(matches!(
        result.target,
        NativeTarget::MetadataCandidate {
            conflicted: true,
            value_retained_only: false,
            ..
        }
    ));
    let view = b.catalog.metadata_for_image(&key)?;
    let field = view.fields.iter().find(|f| f.name == "rating").unwrap();
    assert!(field.conflicted && field.value.is_none());
    assert_eq!(field.candidates.len(), 3);
    assert!(
        field
            .candidates
            .iter()
            .all(|v| v.value == Value::Text("0".into()))
    );
    assert_eq!(choice_count(&b, &key)?, choices);
    assert_eq!(b.catalog.image_metadata_identity(&sibling)?, sibling_before);
    let after = b.catalog.image_metadata_identity(&key)?;
    let reopened_path = b._temp.path().join("catalog");
    drop(b.catalog);
    b.catalog = Catalog::open(reopened_path)?;
    assert_eq!(
        b.catalog.project_migration_organization(None, &flag)?,
        first_flag
    );
    assert_eq!(
        b.catalog.project_migration_organization(None, &label)?,
        first_label
    );
    assert_eq!(
        b.catalog.project_migration_organization(None, &request)?,
        result
    );
    assert_eq!(b.catalog.image_metadata_identity(&key)?, after);
    // An explicit later user resolution/edit works and replay never reauthors it.
    let id = after.image_id;
    b.catalog
        .resolve_metadata(&id, view.revision, "rating", model(&result.target))?;
    let revision = b.catalog.image_metadata_identity(&key)?.metadata_revision;
    b.catalog
        .organize_image(&key, revision, Operation::Rating { value: 5 })?;
    let edited = b.catalog.image_metadata_identity(&key)?;
    b.catalog.project_migration_organization(None, &request)?;
    assert_eq!(b.catalog.image_metadata_identity(&key)?, edited);
    assert_eq!(
        b.catalog
            .metadata_for_image(&key)?
            .fields
            .iter()
            .find(|f| f.name == "rating")
            .unwrap()
            .value,
        Some(Value::Text("5".into()))
    );
    Ok(())
}

#[test]
fn differing_rating_and_label_candidates_rollback_with_receipt() -> Result<()> {
    let mut b = Bed::new(false)?;
    let (key, _) = b.images()?;
    observe(
        &mut b,
        &key,
        "a",
        false,
        &[set("Rating", "1"), set("Label", "Red")],
    )?;
    observe(
        &mut b,
        &key,
        "b",
        false,
        &[set("Rating", "4"), set("Label", "Blue")],
    )?;
    let before = b.catalog.image_metadata_identity(&key)?;
    b.catalog.db.execute_batch("CREATE TRIGGER fail_candidate BEFORE INSERT ON migration_organization BEGIN SELECT RAISE(ABORT,'candidate rollback'); END;")?;
    assert!(b.project(300, Decision::Rating { value: 3 }).is_err());
    assert_eq!(b.catalog.image_metadata_identity(&key)?, before);
    assert_eq!(
        b.catalog
            .metadata_for_image(&key)?
            .fields
            .iter()
            .find(|f| f.name == "rating")
            .unwrap()
            .candidates
            .len(),
        2
    );
    b.catalog.db.execute_batch("DROP TRIGGER fail_candidate;")?;
    for decision in [
        Decision::Rating { value: 3 },
        Decision::Label {
            value: "Green".into(),
        },
    ] {
        assert!(matches!(
            b.project(300, decision)?.target,
            NativeTarget::MetadataCandidate {
                conflicted: true,
                ..
            }
        ));
    }
    assert_eq!(choice_count(&b, &key)?, 0);
    Ok(())
}

#[test]
fn keywords_accumulate_owned_set_keep_siblings_and_stale_choices_explicit() -> Result<()> {
    let mut b = Bed::new(false)?;
    b.keywords()?;
    let (key, sibling) = b.images()?;
    let a = member(&b, 500, 300, 201);
    let z = member(&b, 502, 300, 203);
    let other = member(&b, 503, 301, 201);
    let first = b
        .catalog
        .project_migration_organization(Some(&b.source), &a)?;
    let snapshot = b.catalog.image_metadata_identity(&key)?;
    b.catalog.resolve_metadata(
        &snapshot.image_id,
        snapshot.metadata_revision,
        "hierarchical_keywords",
        model(&first.target),
    )?;
    let second = b
        .catalog
        .project_migration_organization(Some(&b.source), &z)?;
    let view = b.catalog.metadata_for_image(&key)?;
    let field = view
        .fields
        .iter()
        .find(|f| f.name == "hierarchical_keywords")
        .unwrap();
    assert!(field.conflicted && field.value.is_none());
    assert_eq!(field.candidates.len(), 1);
    let Value::List(items) = &field.candidates[0].value else {
        panic!("expected list")
    };
    assert_eq!(
        items,
        &vec!["Animals|Same".to_string(), "Birds|Same".to_string()]
    );
    assert_ne!(model(&first.target), model(&second.target));
    assert!(
        !b.catalog
            .metadata_model(&snapshot.image_id, model(&first.target))?
            .is_empty()
    );
    assert!(b.catalog.metadata_for_image(&sibling)?.fields.is_empty());
    b.catalog
        .project_migration_organization(Some(&b.source), &other)?;
    let copy = b.catalog.metadata_for_image(&sibling)?;
    assert_eq!(
        copy.fields
            .iter()
            .find(|f| f.name == "hierarchical_keywords")
            .unwrap()
            .value,
        Some(Value::List(vec!["Birds|Same".into()]))
    );
    let before = b.catalog.image_metadata_identity(&key)?;
    assert_eq!(b.catalog.project_migration_organization(None, &a)?, first);
    assert_eq!(b.catalog.project_migration_organization(None, &z)?, second);
    assert_eq!(b.catalog.image_metadata_identity(&key)?, before);
    Ok(())
}

#[test]
fn old_keyword_receipt_seeds_new_candidate_and_preserves_local_choice() -> Result<()> {
    let mut b = Bed::new(false)?;
    b.keywords()?;
    let (key, _) = b.images()?;
    let prior = member(&b, 500, 300, 201);
    let next = member(&b, 502, 300, 203);
    // Exercise the old engine and exactly its transaction callback receipt.
    let mut evidence = Evidence::default();
    evidence.source(&b.catalog.db, &prior.origin)?;
    for link in prior.decision.links() {
        verify_unique_link(&b.catalog.db, &mut evidence, &prior.origin, link, &b.source)?;
    }
    let proof = evidence.proof(&prior)?;
    let digest = input_digest(&prior)?;
    let expected = b.catalog.image_metadata_identity(&key)?;
    b.catalog.organize_image_with_commit(
        &key,
        expected.metadata_revision,
        Operation::AddKeyword {
            kind: KeywordKind::Hierarchical,
            path: vec!["Birds".into(), "Same".into()],
        },
        |db, revision| {
            save(
                db,
                &prior,
                &digest,
                &proof,
                NativeTarget::Image {
                    key: key.clone(),
                    revision,
                },
            )?;
            Ok(())
        },
    )?;
    let choices = choice_count(&b, &key)?;
    let result = b
        .catalog
        .project_migration_organization(Some(&b.source), &next)?;
    let id = b.catalog.image_metadata_identity(&key)?.image_id;
    let parsed = xmp::project(&b.catalog.metadata_model(&id, model(&result.target))?)?;
    assert_eq!(
        parsed.fields["hierarchical_keywords"],
        Value::List(vec!["Animals|Same".into(), "Birds|Same".into()])
    );
    assert_eq!(choice_count(&b, &key)?, choices);
    assert_eq!(
        b.catalog
            .metadata_for_image(&key)?
            .fields
            .iter()
            .find(|f| f.name == "hierarchical_keywords")
            .unwrap()
            .value,
        Some(Value::List(vec!["Birds|Same".into()]))
    );
    assert!(matches!(
        b.catalog
            .project_migration_organization(None, &prior)?
            .target,
        NativeTarget::Image { .. }
    ));
    Ok(())
}

#[test]
fn seed_accepts_over_100_fresh_memberships_and_unrelated_incoming_rows() -> Result<()> {
    let mut b = Bed::with_incoming(false, 151)?;
    b.keywords()?;
    let (key, _) = b.images()?;
    let request = member(&b, 500, 300, 201);
    let result = b
        .catalog
        .project_migration_organization(Some(&b.source), &request)?;
    assert!(matches!(
        result.target,
        NativeTarget::MetadataCandidate {
            value_retained_only: false,
            ..
        }
    ));
    let view = b.catalog.metadata_for_image(&key)?;
    assert_eq!(
        view.fields
            .iter()
            .find(|f| f.name == "hierarchical_keywords")
            .unwrap()
            .value,
        Some(Value::List(vec!["Birds|Same".into()]))
    );
    Ok(())
}

#[test]
fn keyword_item_cap_is_sticky_and_never_promotes_partial_set() -> Result<()> {
    let mut b = Bed::new(false)?;
    b.keywords()?;
    let (key, _) = b.images()?;
    let first = member(&b, 500, 300, 201);
    let result = b
        .catalog
        .project_migration_organization(Some(&b.source), &first)?;
    let id = b.catalog.image_metadata_identity(&key)?.image_id;
    let locator:Vec<u8> = b.catalog.db.query_row("SELECT s.locator FROM image_metadata_sources s JOIN metadata_models m ON m.observation_id=s.current_observation WHERE s.asset_id=?1 AND m.id=?2",
        params![id,model(&result.target)],|r|r.get(0))?;
    // Construct the exact native parser boundary without 10,000 incremental
    // edits. This is an in-memory synthetic prior accumulated observation.
    let mut xml = String::from(
        "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:lr='http://ns.adobe.com/lightroom/1.0/'><lr:hierarchicalSubject><rdf:Bag>",
    );
    xml.push_str("<rdf:li>Birds|Same</rdf:li>");
    for i in 0..9_999 {
        xml.push_str(&format!("<rdf:li>term-{i}</rdf:li>"));
    }
    xml.push_str("</rdf:Bag></lr:hierarchicalSubject></rdf:Description></rdf:RDF></x:xmpmeta>");
    b.catalog.retain_image_metadata_id(
        &id,
        &Source {
            kind: "catalog".into(),
            locator,
            display: "synthetic boundary".into(),
            ambiguous: false,
            provenance: serde_json::json!({"synthetic_boundary":true}),
        },
        &candidates::inspection(xml.into_bytes(), false),
        false,
    )?;
    let next = member(&b, 502, 300, 203);
    let capped = b
        .catalog
        .project_migration_organization(Some(&b.source), &next)?;
    assert!(matches!(
        capped.target,
        NativeTarget::MetadataCandidate {
            value_retained_only: true,
            conflicted: true,
            ..
        }
    ));
    let field = b
        .catalog
        .metadata_for_image(&key)?
        .fields
        .into_iter()
        .find(|f| f.name == "hierarchical_keywords")
        .unwrap();
    assert!(field.conflicted && field.candidates[0].ambiguous);
    let Value::List(terms) = &field.candidates[0].value else {
        panic!("expected list")
    };
    assert_eq!(terms.len(), 10_000);
    assert!(!terms.contains(&"Animals|Same".to_string()));
    // A later shorter term is still retained-only; status cannot recover merely
    // because one operation would fit. Use the same image with a new source row.
    let later = member(&b, 504, 300, 201);
    let still_capped = b
        .catalog
        .project_migration_organization(Some(&b.source), &later)?;
    assert!(matches!(
        still_capped.target,
        NativeTarget::MetadataCandidate {
            value_retained_only: true,
            conflicted: true,
            ..
        }
    ));
    let status:String=b.catalog.db.query_row("SELECT o.status FROM metadata_observations o JOIN metadata_models m ON m.observation_id=o.id WHERE m.id=?1",[model(&capped.target)],|r|r.get(0))?;
    assert_eq!(status, "ResourceLimit");
    assert_eq!(
        b.catalog.project_migration_organization(None, &next)?,
        capped
    );
    Ok(())
}

#[test]
fn flat_and_hierarchical_accumulators_do_not_seed_each_other() -> Result<()> {
    let mut b = Bed::new(false)?;
    b.keywords()?;
    b.project(
        205,
        Decision::Keyword {
            name: "Flat".into(),
            keyword_kind: KeywordKind::Flat,
            parent: None,
            decision: DictionaryDecision::Create,
        },
    )?;
    let (key, _) = b.images()?;
    let hierarchy = member(&b, 500, 300, 201);
    b.catalog
        .project_migration_organization(Some(&b.source), &hierarchy)?;
    let flat = member(&b, 505, 300, 205);
    b.catalog
        .project_migration_organization(Some(&b.source), &flat)?;
    let view = b.catalog.metadata_for_image(&key)?;
    assert_eq!(
        view.fields
            .iter()
            .find(|f| f.name == "keywords")
            .unwrap()
            .value,
        Some(Value::List(vec!["Flat".into()]))
    );
    assert_eq!(
        view.fields
            .iter()
            .find(|f| f.name == "hierarchical_keywords")
            .unwrap()
            .value,
        Some(Value::List(vec!["Birds|Same".into()]))
    );
    Ok(())
}
