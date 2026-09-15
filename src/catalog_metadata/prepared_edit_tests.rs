use super::*;
use crate::catalog_edits::VariantKey;

const PACKET: &[u8] = br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:u="https://example.invalid/unknown/" xmp:Rating="2"><xmp:Label rdf:parseType="Resource"><rdf:value>old</rdf:value><u:qualifier>label qualifier</u:qualifier></xmp:Label><dc:subject><rdf:Bag><rdf:li>first</rdf:li></rdf:Bag></dc:subject><dc:description><rdf:Alt><rdf:li xml:lang="x-default">English</rdf:li><rdf:li xml:lang="en-US">English</rdf:li></rdf:Alt></dc:description><u:complex rdf:parseType="Resource"><u:preserve>unrelated structure</u:preserve></u:complex></rdf:Description></rdf:RDF>"#;

fn fixture(root: &Path) -> Result<Catalog> {
    let mut catalog = Catalog::open(root)?;
    catalog.db.execute("INSERT INTO assets(id,location,path_display,state) VALUES('a',?1,'missing original','pending')",[b"missing original".as_slice()])?;
    let path = root.join("synthetic-source.xmp");
    fs::write(&path, PACKET)?;
    let inspection = xmp_packets::inspect_sidecar(&path, &Limits::default())?;
    catalog.retain_metadata(
        "a",
        &Source {
            kind: "sidecar".into(),
            locator: location_bytes(&path),
            display: "synthetic source".into(),
            ambiguous: false,
            provenance: serde_json::json!({"fixture":true}),
        },
        &inspection,
    )?;
    Ok(catalog)
}

fn edits() -> Vec<Edit> {
    vec![
        Edit::Set {
            namespace: xmp::XMP.into(),
            path: "Label".into(),
            value: "green".into(),
        },
        Edit::Remove {
            namespace: xmp::XMP.into(),
            path: "Rating".into(),
        },
        Edit::Append {
            namespace: xmp::DC.into(),
            path: "subject".into(),
            value: "second".into(),
            ordered: false,
        },
        Edit::Localized {
            namespace: xmp::DC.into(),
            path: "description".into(),
            language: "fr-FR".into(),
            value: "Bonjour".into(),
        },
    ]
}

fn prepare(catalog: &Catalog, image: &str) -> Result<PreparedEdit> {
    let view = catalog.metadata(image)?;
    let base = view
        .fields
        .iter()
        .find(|v| v.name == "rating")
        .unwrap()
        .selected_model;
    catalog.prepare_metadata_edit(image, view.revision, base, &edits(), &[])
}

fn state(catalog: &Catalog) -> Result<Vec<i64>> {
    [
        "metadata_blobs",
        "metadata_models",
        "metadata_sources",
        "metadata_observations",
        "metadata_history",
        "metadata_choices",
        "organization_events",
    ]
    .into_iter()
    .map(|table| {
        Ok(catalog
            .db
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    })
    .collect()
}

fn unrelated(bytes: &[u8]) -> Result<String> {
    let mut parsed = xmp::parse(bytes)?;
    for (namespace, path) in [
        (xmp::XMP, "Label"),
        (xmp::XMP, "Rating"),
        (xmp::DC, "subject"),
        (xmp::DC, "description"),
    ] {
        parsed.delete_property(namespace, path)?;
    }
    xmp::canonical(&parsed)
}

#[test]
fn all_four_forms_prepare_without_writes_and_commit_on_inherited_worker() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut catalog = fixture(&temp.path().join("catalog"))?;
    let master = VariantKey::master("a");
    let copy = catalog.create_edit_variant(&master, 0, "copy")?.key;
    let copy_identity = catalog.image_metadata_identity(&copy)?;
    let master_identity = catalog.image_metadata_identity(&master)?;
    let before = state(&catalog)?;
    let edit = prepare(&catalog, &copy_identity.image_id)?;
    assert_eq!(state(&catalog)?, before);
    let handle = catalog.relink_worker_handle()?;
    let change = std::thread::spawn(move || {
        let mut worker = handle.open()?;
        worker.commit_prepared_metadata_edit(edit, |_, _| Ok(()))
    })
    .join()
    .unwrap()?;
    assert_eq!(change.revision, copy_identity.metadata_revision + 1);
    assert_eq!(catalog.image_metadata_identity(&master)?, master_identity);
    let bytes = catalog.metadata_model_for_image(&copy, change.model_ids[0])?;
    let parsed = xmp::parse(&bytes)?;
    assert_eq!(parsed.property(xmp::XMP, "Label").unwrap().value, "green");
    assert!(parsed.property(xmp::XMP, "Rating").is_none());
    assert_eq!(
        parsed.property(xmp::DC, "subject[1]").unwrap().value,
        "first"
    );
    assert_eq!(
        parsed.property(xmp::DC, "subject[2]").unwrap().value,
        "second"
    );
    let fields = xmp::project(&bytes)?.fields;
    let Value::Localized(description) = &fields["description"] else {
        panic!("localized description lost")
    };
    assert_eq!(description["en-US"], "English");
    assert_eq!(description["fr-FR"], "Bonjour");
    assert!(xmp::canonical(&parsed)?.contains("label qualifier"));
    assert_eq!(unrelated(&bytes)?, unrelated(PACKET)?);
    assert_eq!(fs::read(catalog.root.join("synthetic-source.xmp"))?, PACKET);
    Ok(())
}

#[test]
fn every_image_authority_change_rejects_prepared_edit_without_publication() -> Result<()> {
    for change in ["metadata", "pixel", "physical", "source"] {
        let temp = tempfile::tempdir()?;
        let mut catalog = fixture(&temp.path().join("catalog"))?;
        let edit = prepare(&catalog, "a")?;
        match change {
            "metadata" => {
                catalog.edit_metadata(
                    "a",
                    catalog.metadata("a")?.revision,
                    None,
                    &[Edit::Set {
                        namespace: xmp::XMP.into(),
                        path: "Rating".into(),
                        value: "5".into(),
                    }],
                )?;
            }
            "pixel" => {
                catalog.db.execute(
                    "UPDATE catalog_images SET pixel_generation=pixel_generation+1 WHERE id='a'",
                    [],
                )?;
            }
            "physical" => {
                catalog.db.execute(
                    "UPDATE assets SET physical_generation=physical_generation+1 WHERE id='a'",
                    [],
                )?;
            }
            "source" => {
                catalog.db.execute_batch("UPDATE image_shared_state SET epoch=epoch+1 WHERE asset_id='a';UPDATE catalog_images SET applied_shared_epoch=applied_shared_epoch+1 WHERE id='a';")?;
            }
            _ => unreachable!(),
        }
        let before = state(&catalog)?;
        let identity = catalog.image_metadata_identity(&VariantKey::master("a"))?;
        assert!(
            catalog
                .commit_prepared_metadata_edit(edit, |_, _| panic!("stale edit reached callback"))
                .is_err(),
            "{change}"
        );
        assert_eq!(state(&catalog)?, before, "{change}");
        assert_eq!(
            catalog.image_metadata_identity(&VariantKey::master("a"))?,
            identity,
            "{change}"
        );
    }
    Ok(())
}

#[test]
fn different_catalog_and_independent_reopen_cannot_consume_preparation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let catalog = fixture(&temp.path().join("catalog"))?;
    let mut different = fixture(&temp.path().join("different"))?;
    let edit = prepare(&catalog, "a")?;
    let before = state(&different)?;
    assert!(
        different
            .commit_prepared_metadata_edit(edit, |_, _| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("another catalog session")
    );
    assert_eq!(state(&different)?, before);
    let edit = prepare(&catalog, "a")?;
    let before = state(&catalog)?;
    drop(catalog);
    let mut reopened = Catalog::open(temp.path().join("catalog"))?;
    assert!(
        reopened
            .commit_prepared_metadata_edit(edit, |_, _| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("another catalog session")
    );
    assert_eq!(state(&reopened)?, before);
    Ok(())
}

#[test]
fn callback_sees_new_revision_and_failure_rolls_back_entire_commit() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut catalog = fixture(&temp.path().join("catalog"))?;
    let edit = prepare(&catalog, "a")?;
    let expected = edit.expected_revision;
    let before = state(&catalog)?;
    let identity = catalog.image_metadata_identity(&VariantKey::master("a"))?;
    let result = catalog.commit_prepared_metadata_edit(edit, |db, next| {
        assert_eq!(next, expected + 1);
        assert_eq!(revision(db, "a")?, next);
        anyhow::bail!("synthetic organization callback failure")
    });
    assert_eq!(
        result.unwrap_err().to_string(),
        "synthetic organization callback failure"
    );
    assert_eq!(state(&catalog)?, before);
    assert_eq!(
        catalog.image_metadata_identity(&VariantKey::master("a"))?,
        identity
    );
    Ok(())
}

#[test]
fn durable_attempt_replays_exact_result_and_failed_cas_writes_no_receipt() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut catalog = fixture(&temp.path().join("catalog"))?;
    let attempt = uuid::Uuid::new_v4().to_string();
    let digest = blake3::hash(b"prepared edit request").to_hex().to_string();
    let edit = prepare(&catalog, "a")?;
    let change = catalog.commit_prepared_metadata_edit_with_receipt(edit, &attempt, &digest)?;
    let receipt = catalog
        .metadata_write_receipt(&attempt)?
        .context("durable metadata receipt")?;
    assert_eq!(receipt.request_digest, digest);
    assert_eq!(receipt.kind, "edit");
    assert_eq!(receipt.result["revision"], change.revision);
    assert!(
        crate::catalog_metadata_write::existing(
            &catalog.db,
            &attempt,
            &blake3::hash(b"different request").to_hex().to_string(),
        )
        .unwrap_err()
        .to_string()
        .contains("different request")
    );

    let stale = prepare(&catalog, "a")?;
    let view = catalog.metadata("a")?;
    catalog.edit_metadata(
        "a",
        view.revision,
        None,
        &[Edit::Set {
            namespace: xmp::XMP.into(),
            path: "Rating".into(),
            value: "4".into(),
        }],
    )?;
    let failed_attempt = uuid::Uuid::new_v4().to_string();
    let failed_digest = blake3::hash(b"stale prepared edit").to_hex().to_string();
    assert!(
        catalog
            .commit_prepared_metadata_edit_with_receipt(stale, &failed_attempt, &failed_digest,)
            .is_err()
    );
    assert!(catalog.metadata_write_receipt(&failed_attempt)?.is_none());
    Ok(())
}
