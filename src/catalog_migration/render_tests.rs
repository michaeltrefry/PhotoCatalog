// Included only inside images::tests to reuse its sealed-source custody fixture.
// These tests establish native recipe parameter/pixel semantics, not Adobe
// appearance equivalence, raw decoding, camera white balance, or GPU behavior.
mod render_acceptance {
    use super::*;
    use crate::{edit, lightroom::adobe};

    fn render_stored(catalog: &Catalog, key: &VariantKey) -> Result<Vec<[f32; 4]>> {
        // Exact binary fractions include HDR, signed channels, and straight alpha.
        let input = edit::fixture(2, 1, vec![[0.125, 0.25, 0.5, 0.75], [2.0, -0.25, 0.0, 0.0]]);
        let stored = catalog.edit_variant(key)?;
        let recipe = stored.recipe.validate()?;
        let rendered = edit::render_recipe(
            &input,
            &recipe,
            edit::RenderPurpose::ExportExact,
            edit::RenderLimits::default(),
            &(),
        )?;
        assert!(rendered.exact());
        assert_eq!(rendered.recipe_digest(), recipe.digest());
        Ok(rendered.as_rendered().pixels.clone())
    }

    #[test]
    fn prepared_wrapped_current_projection_requires_atomic_writer_and_retains_replay() -> Result<()> {
        use crate::catalog_migration::metadata::{commit_current_develop_projection, current_develop_input_digest};
        let mut t = Test::with_source_edit(false, false, |db, revision| {
            db.execute("UPDATE rows SET cells_json=?1 WHERE revision=?2 AND source_id='source-30'", params![serde_json::to_string(&vec![Cell::Integer(30),Cell::Text(b"s={ProcessVersion='11.0',Exposure2012=1,UnknownPlugin={opaque='keep'}}".to_vec())]).unwrap(),revision]).unwrap();
        })?;
        t.original()?;
        t.project(20)?;
        let old = develop(&t,20,30);
        let mut explicit = old.clone();
        explicit.settings_path=vec![adobe::Key::Name("explicit".into())];
        assert_eq!(t.catalog.prepare_migration_current_develop(explicit.clone())?.settings_path,explicit.settings_path);
        let selected=t.catalog.prepare_migration_current_develop(old.clone())?;
        assert_eq!(selected.settings_path,vec![adobe::Key::Name("s".into())]);
        assert_ne!(current_develop_input_digest(&old)?,current_develop_input_digest(&selected)?);
        let prepared=t.catalog.prepare_current_develop_projection(&t.source,&selected)?;
        let key=prepared.key().clone();
        assert_eq!(prepared.result().state,"translated_with_appearance_gaps");
        assert!(commit_current_develop_projection(&t.catalog.db,&prepared).is_err());
        let before=t.catalog.edit_variant(&key)?;
        {
            let tx=t.catalog.db.transaction()?;
            let result=commit_current_develop_projection(&tx,&prepared)?;
            assert!(result.edit_revision.is_some());
            // Simulates an enclosing repair ledger/cursor failure.
            tx.rollback()?;
        }
        assert_eq!(t.catalog.edit_variant(&key)?.revision,before.revision);
        assert_eq!(t.catalog.db.query_row("SELECT count(*) FROM migration_metadata WHERE slot='current_develop'",[],|r|r.get::<_,i64>(0))?,0);
        let result={
            let tx=t.catalog.db.transaction()?;
            let result=commit_current_develop_projection(&tx,&prepared)?;
            tx.commit()?;
            result
        };
        assert_eq!(render_stored(&t.catalog,&key)?,vec![[0.25,0.5,1.0,0.75],[4.0,-0.5,0.0,0.0]]);
        let edited=edit::Recipe::V1(edit::RecipeV1{exposure_ev:2.0,..Default::default()});
        t.catalog.save_edit_recipe(&key,result.edit_revision.unwrap(),&edited)?;
        {
            let tx=t.catalog.db.transaction()?;
            assert!(commit_current_develop_projection(&tx,&prepared).is_err());
            tx.rollback()?;
        }
        let replay=t.catalog.project_migration_current_develop(None,&selected)?;
        assert_eq!(replay.input_digest,result.input_digest);
        assert_eq!(t.catalog.edit_variant(&key)?.recipe,edited);
        Ok(())
    }

    #[test]
    fn selected_current_settings_render_independent_stored_variant_pixels() -> Result<()> {
        let mut t = Test::new()?;
        t.original()?;
        t.project(20)?;
        t.project(21)?;
        let master_request = develop(&t, 20, 30);
        let copy_request = develop(&t, 21, 31);
        let master = t
            .catalog
            .project_migration_current_develop(Some(&t.source), &master_request)?;
        let copy = t
            .catalog
            .project_migration_current_develop(Some(&t.source), &copy_request)?;
        assert_eq!(master.image.asset_id, copy.image.asset_id);
        assert_ne!(master.image, copy.image);
        for result in [&master, &copy] {
            let extraction = result
                .extraction
                .as_ref()
                .context("current extraction absent")?;
            assert_eq!(extraction.input.association, adobe::Association::Current);
            assert!(!extraction.adobe_rendering_equivalent);
            assert_eq!(result.state, "translated_with_appearance_gaps");
        }
        let unknown = master
            .extraction
            .as_ref()
            .unwrap()
            .properties
            .iter()
            .find(|p| p.name == "opaque")
            .unwrap();
        assert_eq!(unknown.disposition, adobe::Disposition::RetainedOnly);
        let contrast = copy
            .extraction
            .as_ref()
            .unwrap()
            .properties
            .iter()
            .find(|p| p.name == "Contrast2012")
            .unwrap();
        assert_eq!(contrast.disposition, adobe::Disposition::RetainedOnly);
        // The oracle is independent of the renderer's exposure implementation:
        // +1 EV doubles linear channels; -1 EV halves them, including signed/HDR.
        let master_pixels = vec![[0.25, 0.5, 1.0, 0.75], [4.0, -0.5, 0.0, 0.0]];
        let copy_pixels = vec![[0.0625, 0.125, 0.25, 0.75], [1.0, -0.125, 0.0, 0.0]];
        assert_eq!(render_stored(&t.catalog, &master.image)?, master_pixels);
        assert_eq!(render_stored(&t.catalog, &copy.image)?, copy_pixels);

        // Reload durable catalog state; current source pointers/recipes need not
        // be re-extracted to render. Both variants remain independently addressable.
        drop(t.catalog);
        let mut catalog = Catalog::open(t._temp.path().join("catalog"))?;
        assert_eq!(render_stored(&catalog, &master.image)?, master_pixels);
        assert_eq!(render_stored(&catalog, &copy.image)?, copy_pixels);
        let edited = edit::Recipe::V1(edit::RecipeV1 {
            exposure_ev: 2.0,
            ..Default::default()
        });
        catalog.save_edit_recipe(&master.image, master.edit_revision.unwrap(), &edited)?;
        assert_eq!(
            render_stored(&catalog, &master.image)?,
            vec![[0.5, 1.0, 2.0, 0.75], [8.0, -1.0, 0.0, 0.0]]
        );
        assert_eq!(render_stored(&catalog, &copy.image)?, copy_pixels);
        // Idempotent source replay must not reinstall the old +1 recipe over +2.
        catalog.project_migration_current_develop(None, &master_request)?;
        assert_eq!(catalog.edit_variant(&master.image)?.recipe, edited);
        assert_eq!(render_stored(&catalog, &copy.image)?, copy_pixels);
        Ok(())
    }

    #[test]
    fn unsupported_current_and_retained_history_cannot_silently_change_rendered_pixels()
    -> Result<()> {
        const HISTORICAL: &[u8] =
            b"{ProcessVersion='11.0',Exposure2012=4,UnknownPlugin={opaque='history'}}";
        const UNSUPPORTED: &[u8] =
            b"{ProcessVersion='future-unknown',Exposure2012=4,UnknownPlugin={opaque='current'}}";
        let mut t = Test::with_source_edit(false, false, |db, revision| {
            db.execute(
                "UPDATE rows SET cells_json=?1 WHERE revision=?2 AND source_id='source-30'",
                params![
                    serde_json::to_string(&vec![
                        Cell::Integer(30),
                        Cell::Text(UNSUPPORTED.to_vec())
                    ])
                    .unwrap(),
                    revision
                ],
            )
            .unwrap();
            db.execute("INSERT INTO tables(revision,name,columns_json,key_json,schema_json,category,expected,retained,state) VALUES(?1,'Adobe_libraryImageDevelopHistoryStep','[\"id_local\",\"image\",\"text\"]','[]','{}','fixture',1,1,'complete')",[revision]).unwrap();
            db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?1,'source-50','Adobe_libraryImageDevelopHistoryStep',?2,?3)",params![revision,serde_json::to_string(&vec![Cell::Integer(50)]).unwrap(),serde_json::to_string(&vec![Cell::Integer(50),Cell::Integer(20),Cell::Text(HISTORICAL.to_vec())]).unwrap()]).unwrap();
            db.execute("INSERT INTO entities VALUES(?1,'source-50','Adobe_libraryImageDevelopHistoryStep',?2,NULL,'{}')",params![revision,serde_json::to_string(&Cell::Integer(50)).unwrap()]).unwrap();
            db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,'source-50','image','Adobe_images',?2)",params![revision,serde_json::to_string(&Cell::Integer(20)).unwrap()]).unwrap();
        })?;
        t.original()?;
        t.project(20)?;
        let current_request = develop(&t, 20, 30);
        let result = t
            .catalog
            .project_migration_current_develop(Some(&t.source), &current_request)?;
        let extraction = result
            .extraction
            .as_ref()
            .context("unsupported extraction absent")?;
        assert_eq!(result.state, "retained_only");
        assert_eq!(extraction.input.association, adobe::Association::Current);
        assert!(extraction.contribution.exposure_ev.is_none());
        assert!(!extraction.adobe_rendering_equivalent);
        assert!(
            extraction
                .properties
                .iter()
                .any(|p| p.name == "Exposure2012"
                    && p.disposition == adobe::Disposition::RetainedOnly)
        );
        let neutral = vec![[0.125, 0.25, 0.5, 0.75], [2.0, -0.25, 0.0, 0.0]];
        assert_eq!(render_stored(&t.catalog, &result.image)?, neutral);

        // Extract from the exact retained historical row, not synthetic detached
        // text claiming to be history. Never pass its recipe into the native edit.
        let mut proof = Evidence::default();
        let historical = columns(
            &t.catalog.db,
            &mut proof,
            &t.rows[&50],
            t.tables["Adobe_libraryImageDevelopHistoryStep"],
        )?;
        let Cell::Text(body) = &historical["text"] else {
            panic!("historical text expected")
        };
        assert_eq!(body, HISTORICAL);
        let retained = adobe::extract(
            body,
            adobe::Input {
                source_id: t.rows[&50].source.identity()?,
                revision: t.rows[&50].source.capture_revision.clone(),
                locator: "retained history/text".into(),
                payload_blake3: blake3::hash(body).to_hex().to_string(),
                payload_bytes: body.len() as u64,
                format: adobe::Format::CatalogData,
                source_kind: adobe::SourceKind::Raw,
                association: adobe::Association::Historical,
                settings_path: vec![],
                as_shot_available: false,
            },
            adobe::Limits::default(),
        )?;
        assert!(retained.contribution.exposure_ev.is_none());
        assert!(retained.properties.iter().any(|p| p.name == "opaque"));
        assert!(!retained.adobe_rendering_equivalent);
        let before = t.catalog.edit_variant(&result.image)?;
        let mut invalid = current_request.clone();
        invalid.settings.target = t.rows[&50].clone();
        invalid.settings.target_entity_record = t.entities["source-50"];
        invalid.settings_table = t.tables["Adobe_libraryImageDevelopHistoryStep"];
        invalid.expected_edit_revision = before.revision;
        assert!(
            t.catalog
                .project_migration_current_develop(Some(&t.source), &invalid)
                .is_err()
        );
        assert_eq!(t.catalog.edit_variant(&result.image)?.recipe, before.recipe);
        assert_eq!(render_stored(&t.catalog, &result.image)?, neutral);
        Ok(())
    }
}
