use anyhow::{Result, ensure};
use photocatalog::{
    Catalog,
    catalog_storage::{PathReference, RelinkScope},
    organization::{BatchItem, Flag, KeywordKind, Operation},
    organization_search::{Direction, Query, Sort},
    storage_volume::NativePath,
};
use rusqlite::{Connection, params};
use std::{
    fs,
    path::{Path, PathBuf},
};
fn db(root: &Path) -> Result<Connection> {
    Ok(Connection::open(root.join("catalog.sqlite3"))?)
}
fn setup() -> Result<(tempfile::TempDir, PathBuf, PathBuf, Catalog)> {
    let temp = tempfile::Builder::new().tempdir_in(std::env::temp_dir().canonicalize()?)?;
    let root = temp.path().join("catalog");
    let photos = temp.path().join("photos");
    fs::create_dir(&photos)?;
    let cat = Catalog::open(&root)?;
    Ok((temp, root, photos, cat))
}
fn photo(path: &Path) -> Result<()> {
    image::RgbImage::from_pixel(8, 8, image::Rgb([40, 60, 100])).save(path)?;
    Ok(())
}
fn packet(rating: u8) -> String {
    format!(
        r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="photo" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:lr="http://ns.adobe.com/lightroom/1.0/" xmlns:u="urn:untouched" xmp:Rating="{rating}" xmp:Label="source label"><u:opaque rdf:parseType="Resource"><u:child rdf:parseType="Resource"><rdf:value>payload</rdf:value><u:q>retain</u:q></u:child></u:opaque><lr:hierarchicalSubject><rdf:Bag><rdf:li rdf:parseType="Resource"><rdf:value>Z|Keep</rdf:value><u:key>keep</u:key></rdf:li><rdf:li rdf:parseType="Resource"><rdf:value>A|Target</rdf:value><u:key>target</u:key></rdf:li><rdf:li rdf:parseType="Resource"><rdf:value>A|Target|Kid</rdf:value><u:key>kid</u:key></rdf:li><rdf:li rdf:parseType="Resource"><rdf:value>A|Targeted</rdf:value><u:key>other</u:key></rdf:li></rdf:Bag></lr:hierarchicalSubject></rdf:Description></rdf:RDF>"#
    )
}
fn apply(cat: &mut Catalog, asset: &str, op: Operation) -> Result<i64> {
    let revision = cat.metadata(asset)?.revision;
    cat.organize_asset(asset, revision, op)
}
fn finish_index(cat: &mut Catalog) -> Result<()> {
    while cat.organization_index(100)?.pending {}
    Ok(())
}
fn all(cat: &mut Catalog, q: &Query, limit: usize, scan: usize) -> Result<Vec<i64>> {
    let mut cursor = None;
    let mut result = Vec::new();
    for _ in 0..10000 {
        let page = cat.search(q, cursor.as_ref(), limit, scan)?;
        ensure!(page.scanned <= scan);
        result.extend(page.rows.iter().map(|a| a.sequence));
        if page.exhausted {
            return Ok(result);
        }
        ensure!(page.has_more && page.next.is_some());
        cursor = page.next;
    }
    anyhow::bail!("query did not finish")
}
fn synthetic(count: i64) -> Result<(tempfile::TempDir, PathBuf, Catalog)> {
    let (temp, root, _, mut cat) = setup()?;
    let mut conn = db(&root)?;
    let tx = conn.transaction()?;
    for i in 1..=count {
        let path = PathBuf::from(format!("/synthetic/folder{}/file{i:05}.jpg", i % 5));
        let native = NativePath::from_path(&path);
        let location = serde_json::to_vec(&native)?;
        let metadata = serde_json::json!({"format":if i%2==0{"JPEG"}else{"DNG"},"width":8,"height":8,"orientation":1,"camera_make":"fixture","camera_model":format!("camera{}",i%3),"captured_at":format!("2024:01:{:02} 12:00:00",i%28+1),"lens":format!("lens{}",i%4),"preview_source":"synthetic metadata only"});
        tx.execute("INSERT INTO assets(sequence,id,location,path_display,state,metadata) VALUES(?1,?2,?3,?4,'pending',?5)",params![i,format!("asset{i}"),location,path.to_string_lossy(),metadata.to_string()])?;
        // Synthetic projection evidence exercises the shipped index/query path;
        // it deliberately claims no image decode or real packet-parser coverage.
        tx.execute(
            "INSERT INTO metadata_assets VALUES(?1,0)",
            [format!("asset{i}")],
        )?;
        for (field, value) in [
            (
                "rating",
                photocatalog::xmp::Value::Text((i % 6).to_string()),
            ),
            (
                "label",
                photocatalog::xmp::Value::Text(if i % 2 == 0 { "red" } else { "blue" }.into()),
            ),
            (
                "title",
                photocatalog::xmp::Value::Text(
                    if i % 5 == 0 {
                        "blue sunset"
                    } else {
                        "green mountain"
                    }
                    .into(),
                ),
            ),
            (
                "hierarchical_keywords",
                photocatalog::xmp::Value::List(vec![format!("H|Group{}", i % 7)]),
            ),
        ] {
            tx.execute(
                "INSERT INTO metadata_effective VALUES(?1,?2,?3,0,NULL)",
                params![format!("asset{i}"), field, serde_json::to_string(&value)?],
            )?;
        }
        tx.execute(
            "INSERT INTO organization_flags VALUES(?1,?2,'{\"fixture\":true}')",
            params![
                i,
                match i % 3 {
                    0 => "reject",
                    1 => "pick",
                    _ => "unflagged",
                }
            ],
        )?;
    }
    tx.commit()?;
    drop(conn);
    finish_index(&mut cat)?;
    Ok((temp, root, cat))
}

#[test]
fn full_mixed_filter_oracle_and_budget_exhaustion_do_not_claim_empty_library() -> Result<()> {
    let (_temp, root, mut cat) = synthetic(1500)?;
    let keyword:i64=db(&root)?.query_row("SELECT id FROM organization_keywords WHERE kind='hierarchical' AND path='[\"H\",\"Group1\"]'",[],|r|r.get(0))?;
    let query = Query {
        text: Some("blue sunset".into()),
        keyword: Some(keyword),
        rating: Some(4),
        flag: Some(Flag::Pick),
        label: Some("red".into()),
        date_from: Some("2024-01-05".into()),
        date_until: Some("2024-01-20".into()),
        camera_make: Some("fixture".into()),
        camera: Some("camera1".into()),
        lens: Some("lens0".into()),
        format: Some("jpeg".into()),
        ..Query::default()
    };
    let first = cat.search(&query, None, 7, 32)?;
    ensure!(
        first.rows.is_empty() && !first.exhausted && !first.page_complete && first.scanned == 32
    );
    let expected: Vec<i64> = (1..=1500)
        .filter(|i| {
            i % 5 == 0
                && i % 7 == 1
                && i % 6 == 4
                && i % 3 == 1
                && i % 2 == 0
                && i % 4 == 0
                && (4..19).contains(&(i % 28))
        })
        .collect();
    ensure!(all(&mut cat, &query, 7, 32)? == expected);
    ensure!(expected == vec![400, 820, 1240]);
    for plan in cat.explain_search(&query, None, 32)? {
        ensure!(!plan.contains("USE TEMP B-TREE FOR ORDER BY"), "{plan}");
    }
    Ok(())
}
#[test]
fn stable_sort_ties_and_all_direction_cursors_match_independent_order() -> Result<()> {
    let (_temp, _root, mut cat) = synthetic(211)?;
    for sort in [Sort::Sequence, Sort::Capture, Sort::Filename, Sort::Rating] {
        for direction in [Direction::Ascending, Direction::Descending] {
            let mut expected: Vec<i64> = (1..=211).collect();
            expected.sort_by_key(|i| match sort {
                Sort::Sequence | Sort::Filename => (*i, *i),
                Sort::Capture => (i % 28, *i),
                Sort::Rating => (i % 6, *i),
            });
            if direction == Direction::Descending {
                expected.reverse();
            }
            let q = Query {
                sort,
                direction,
                ..Query::default()
            };
            ensure!(all(&mut cat, &q, 13, 23)? == expected);
        }
    }
    Ok(())
}
#[test]
fn snapshots_hold_cursor_boundary_stable_under_import_and_edit_and_expire_without_polling()
-> Result<()> {
    let (_temp, root, mut cat) = synthetic(80)?;
    let query = Query {
        sort: Sort::Capture,
        ..Query::default()
    };
    let expected = all(&mut cat, &query, 7, 15)?;
    let mut session = cat.search_session(query.clone(), 30)?;
    let first = session.next_page(7, 15)?;
    let mut got: Vec<_> = first.rows.iter().map(|r| r.sequence).collect();
    let serial = cat.search(&query, None, 7, 15)?.next.unwrap();
    let conn = db(&root)?;
    conn.execute("UPDATE assets SET metadata=json_set(metadata,'$.captured_at','2099:01:01 00:00:00') WHERE sequence=28",[])?;
    drop(conn);
    let mut writer = Catalog::open(&root)?;
    finish_index(&mut writer)?;
    let new_photos = root.parent().unwrap().join("concurrent_import");
    fs::create_dir(&new_photos)?;
    photo(&new_photos.join("new.jpg"))?;
    writer.import(&new_photos, None, |_| Ok(()))?;
    ensure!(writer.search(&Query::default(), None, 100, 100)?.rows.len() == 81);
    ensure!(
        cat.search(&query, Some(&serial), 7, 15)
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    loop {
        let page = session.next_page(7, 15)?;
        got.extend(page.rows.iter().map(|r| r.sequence));
        if page.exhausted {
            break;
        }
    }
    ensure!(got == expected);
    session.close()?;
    let mut expired = cat.search_session(Query::default(), 1)?;
    expired.next_page(1, 1)?;
    db(&root)?.execute(
        "UPDATE assets SET path_display='new name' WHERE sequence=1",
        [],
    )?;
    finish_index(&mut writer)?;
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let busy: i64 = db(&root)?.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
    ensure!(busy == 0, "expired session retained its WAL reader");
    ensure!(expired.next_page(1, 1).is_err());
    Ok(())
}
#[test]
fn hierarchical_moves_keep_unsorted_item_qualifiers_and_unrelated_properties() -> Result<()> {
    let (_temp, _root, photos, mut cat) = setup()?;
    photo(&photos.join("one.jpg"))?;
    let xml = packet(3);
    ensure!(
        photocatalog::xmp::project(xml.as_bytes())?
            .fields
            .contains_key("hierarchical_keywords")
    );
    fs::write(photos.join("one.xmp"), &xml)?;
    cat.import(&photos, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    apply(&mut cat, &asset.id, Operation::Rating { value: 5 })?;
    apply(
        &mut cat,
        &asset.id,
        Operation::Label {
            value: "mine".into(),
        },
    )?;
    apply(
        &mut cat,
        &asset.id,
        Operation::MoveKeyword {
            from: vec!["A".into(), "Target".into()],
            to: vec!["B".into(), "Renamed".into()],
        },
    )?;
    let view = cat.metadata(&asset.id)?;
    let hier = view
        .fields
        .iter()
        .find(|f| f.name == "hierarchical_keywords")
        .unwrap();
    let model = hier.selected_model.unwrap();
    let bytes = cat.metadata_model(&asset.id, model)?;
    let meta = photocatalog::xmp::parse(&bytes)?;
    let ns = "http://ns.adobe.com/lightroom/1.0/";
    let expected = [
        ("Z|Keep", "keep"),
        ("B|Renamed", "target"),
        ("B|Renamed|Kid", "kid"),
        ("A|Targeted", "other"),
    ];
    let mut actual = Vec::new();
    for i in 1..=meta.array_len(ns, "hierarchicalSubject") {
        actual.push((
            meta.array_item(ns, "hierarchicalSubject", i as i32)
                .unwrap()
                .value,
            meta.qualifier(
                ns,
                &format!("hierarchicalSubject[{i}]"),
                "urn:untouched",
                "key",
            )
            .unwrap()
            .value,
        ));
    }
    actual.sort();
    let mut expected: Vec<_> = expected
        .iter()
        .map(|(v, q)| (v.to_string(), q.to_string()))
        .collect();
    expected.sort();
    ensure!(actual == expected, "{actual:?}");
    ensure!(
        meta.property("urn:untouched", "opaque/u:child")
            .unwrap()
            .value
            == "payload"
    );
    ensure!(
        view.fields
            .iter()
            .find(|f| f.name == "rating")
            .unwrap()
            .value
            == Some(photocatalog::xmp::Value::Text("5".into()))
    );
    ensure!(fs::read_to_string(photos.join("one.xmp"))? == xml);
    let roots = cat.organization_keywords(KeywordKind::Hierarchical, None, 0, 100)?;
    let b = roots.iter().find(|k| k.name == "B").unwrap();
    ensure!(
        all(
            &mut cat,
            &Query {
                keyword: Some(b.id),
                ..Query::default()
            },
            10,
            10
        )? == vec![asset.sequence]
    );
    ensure!(
        all(
            &mut cat,
            &Query {
                keyword: Some(b.id),
                keyword_direct: true,
                ..Query::default()
            },
            10,
            10
        )?
        .is_empty()
    );
    ensure!(
        cat.organize_asset(
            &asset.id,
            view.revision,
            Operation::MoveKeyword {
                from: vec!["B".into()],
                to: vec!["B".into(), "Child".into()]
            }
        )
        .is_err()
    );
    Ok(())
}
#[test]
fn collection_flags_restart_relink_and_xmp_choice_refresh_stay_consistent() -> Result<()> {
    let (temp, root, photos, mut cat) = setup()?;
    photo(&photos.join("one.jpg"))?;
    fs::write(photos.join("one.xmp"), packet(3))?;
    cat.import(&photos, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let collection = cat.create_collection(
        "Selected",
        serde_json::json!({"import_source":"explicit fixture"}),
    )?;
    apply(
        &mut cat,
        &asset.id,
        Operation::AddCollection {
            collection: collection.clone(),
        },
    )?;
    apply(&mut cat, &asset.id, Operation::Flag { value: Flag::Pick })?;
    apply(&mut cat, &asset.id, Operation::Rating { value: 5 })?;
    fs::write(photos.join("one.xmp"), packet(1))?;
    cat.import(&photos, None, |_| Ok(()))?;
    ensure!(
        all(
            &mut cat,
            &Query {
                collection: Some(collection.clone()),
                flag: Some(Flag::Pick),
                rating: Some(5),
                ..Query::default()
            },
            10,
            10
        )? == vec![asset.sequence]
    );
    let new = temp.path().join("moved");
    fs::rename(&photos, &new)?;
    let plan = cat.begin_relink(RelinkScope::Prefix {
        from: PathReference::native(&photos),
        destinations: vec![NativePath::from_path(&new)],
    })?;
    while cat.relink_plan(&plan.id)?.state == "preparing" {
        cat.prepare_relink_batch(&plan.id, 10)?;
    }
    cat.apply_relink(&plan.id)?;
    drop(cat);
    let mut cat = Catalog::open(&root)?;
    ensure!(
        all(
            &mut cat,
            &Query {
                collection: Some(collection),
                rating: Some(5),
                ..Query::default()
            },
            10,
            10
        )? == vec![asset.sequence]
    );
    let row = cat.search(&Query::default(), None, 10, 10)?.rows.remove(0);
    let folder = row.folder.unwrap();
    let stored: String = db(&root)?.query_row(
        "SELECT locator FROM organization_folders WHERE id=?",
        [folder],
        |r| r.get(0),
    )?;
    ensure!(serde_json::from_str::<NativePath>(&stored)? == NativePath::from_path(&new));
    cat.undo_relink(&plan.id)?;
    ensure!(cat.search(&Query::default(), None, 10, 10)?.rows[0].asset_id == asset.id);
    Ok(())
}
#[test]
fn batches_are_exact_atomic_per_asset_restartable_and_failed_revisions_require_review() -> Result<()>
{
    let (_temp, root, photos, mut cat) = setup()?;
    for name in ["a", "b", "c"] {
        photo(&photos.join(format!("{name}.jpg")))?;
    }
    cat.import(&photos, None, |_| Ok(()))?;
    let assets = cat.browse(0, 10)?;
    let job = cat.begin_organization_batch(Operation::Rating { value: 4 })?;
    let items = assets
        .iter()
        .map(|a| {
            Ok(BatchItem {
                asset_id: a.id.clone(),
                expected_revision: cat.metadata(&a.id)?.revision,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    cat.append_organization_batch(&job.id, &items[..2])?;
    cat.append_organization_batch(&job.id, &items[1..])?;
    ensure!(cat.organization_job(&job.id)?.pending == 3);
    cat.seal_organization_batch(&job.id)?;
    let failed = cat.step_organization_batch_with(&job.id, || {
        Err(anyhow::anyhow!("injected disk exhaustion"))
    })?;
    ensure!(failed.state == "paused" && failed.failed == 1 && failed.applied == 0);
    ensure!(cat.metadata(&items[0].asset_id)?.revision == items[0].expected_revision);
    cat.review_organization_item(
        &job.id,
        assets[0].sequence,
        Some(items[0].expected_revision),
    )?;
    let first = cat.step_organization_batch(&job.id)?;
    ensure!(first.applied == 1 && first.pending == 2);
    drop(cat);
    let mut cat = Catalog::open(&root)?;
    apply(
        &mut cat,
        &items[1].asset_id,
        Operation::Label {
            value: "concurrent".into(),
        },
    )?;
    ensure!(cat.step_organization_batch(&job.id)?.state == "paused");
    cat.review_organization_item(&job.id, assets[1].sequence, None)?;
    let result = cat.step_organization_batch(&job.id)?;
    ensure!(
        result.state == "complete"
            && result.applied == 2
            && result.skipped == 1
            && result.pending == 0
            && result.failed == 0
    );
    let audit = cat.organization_events(0, 100)?;
    ensure!(audit.iter().any(|v| v["action"] == "failed"));
    ensure!(
        cat.organization_job_items(&job.id, assets[0].sequence, 1)?
            .len()
            == 1
    );
    let revisions: Vec<_> = items
        .iter()
        .map(|i| cat.metadata(&i.asset_id).map(|v| v.revision))
        .collect::<Result<_>>()?;
    ensure!(cat.step_organization_batch(&job.id).is_err());
    ensure!(
        items
            .iter()
            .map(|i| cat.metadata(&i.asset_id).map(|v| v.revision))
            .collect::<Result<Vec<_>>>()?
            == revisions
    );
    Ok(())
}
#[test]
fn source_exif_lens_is_indexed_and_older_metadata_json_remains_readable() -> Result<()> {
    let (_temp, _root, photos, mut cat) = setup()?;
    let path = photos.join("lens.jpg");
    photo(&path)?;
    let jpeg = fs::read(&path)?;
    let lens = b"Independent 50mm prime\0";
    let mut tiff = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
    tiff.extend(1u16.to_le_bytes());
    tiff.extend(0x8769u16.to_le_bytes());
    tiff.extend(4u16.to_le_bytes());
    tiff.extend(1u32.to_le_bytes());
    tiff.extend(26u32.to_le_bytes());
    tiff.extend(0u32.to_le_bytes());
    tiff.extend(1u16.to_le_bytes());
    tiff.extend(0xa434u16.to_le_bytes());
    tiff.extend(2u16.to_le_bytes());
    tiff.extend((lens.len() as u32).to_le_bytes());
    tiff.extend(44u32.to_le_bytes());
    tiff.extend(0u32.to_le_bytes());
    tiff.extend(lens);
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend(tiff);
    let mut output = vec![0xff, 0xd8, 0xff, 0xe1];
    output.extend(((payload.len() + 2) as u16).to_be_bytes());
    output.extend(payload);
    output.extend(&jpeg[2..]);
    fs::write(&path, output)?;
    cat.import(&photos, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    ensure!(asset.metadata.as_ref().unwrap().lens.as_deref() == Some("Independent 50mm prime"));
    ensure!(
        all(
            &mut cat,
            &Query {
                lens: Some("Independent 50mm prime".into()),
                ..Query::default()
            },
            10,
            10
        )? == vec![asset.sequence]
    );
    let mut legacy = serde_json::to_value(asset.metadata.unwrap())?;
    legacy.as_object_mut().unwrap().remove("lens");
    ensure!(
        serde_json::from_value::<photocatalog::Metadata>(legacy)?
            .lens
            .is_none()
    );
    Ok(())
}

#[test]
fn invalid_text_and_changed_query_cursors_are_explicit() -> Result<()> {
    let (_temp, _root, mut cat) = synthetic(5)?;
    ensure!(
        cat.search(
            &Query {
                text: Some("   ".into()),
                ..Query::default()
            },
            None,
            1,
            1
        )
        .unwrap_err()
        .to_string()
        .contains("1..16 terms")
    );
    let first = cat.search(&Query::default(), None, 1, 1)?;
    ensure!(
        cat.search(
            &Query {
                rating: Some(1),
                ..Query::default()
            },
            first.next.as_ref(),
            1,
            1
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn text_driving_and_residual_filters_match_oracle_in_both_directions() -> Result<()> {
    let (_temp, _root, mut cat) = synthetic(507)?;
    for sort in [Sort::Sequence, Sort::Capture, Sort::Rating] {
        for direction in [Direction::Ascending, Direction::Descending] {
            let query = Query {
                text: Some("blu suns".into()),
                lens: Some("lens0".into()),
                sort,
                direction,
                ..Query::default()
            };
            let mut expected: Vec<i64> = (1..=507).filter(|i| i % 20 == 0).collect();
            expected.sort_by_key(|i| match sort {
                Sort::Capture => (i % 28, *i),
                Sort::Rating => (i % 6, *i),
                _ => (*i, *i),
            });
            if direction == Direction::Descending {
                expected.reverse();
            }
            ensure!(all(&mut cat, &query, 9, 17)? == expected);
            let p = cat.search(&query, None, 9, 17)?;
            ensure!(
                p.sorts == 0,
                "unexpected query sort: {:?}",
                cat.explain_search(&query, None, 17)?
            );
        }
    }
    Ok(())
}

#[test]
fn migration_backfill_is_paged_restartable_and_unknown_projection_is_not_searchable() -> Result<()>
{
    let (_temp, root, mut cat) = synthetic(23)?;
    // Recreate the migration checkpoint with existing projection rows absent.
    // Production migration creates this exact high-water checkpoint before any backfill.
    db(&root)?.execute_batch("DELETE FROM organization_assets; DELETE FROM organization_text; UPDATE organization_state SET backfill_after=0,backfill_high=23;")?;
    ensure!(cat.search(&Query::default(), None, 10, 10).is_err());
    let first = cat.organization_index(7)?;
    ensure!(first.processed == 7 && first.backfill_after == 7 && first.pending);
    drop(cat);
    let mut cat = Catalog::open(&root)?;
    let second = cat.organization_index(7)?;
    ensure!(second.processed == 7 && second.backfill_after == 14);
    finish_index(&mut cat)?;
    ensure!(all(&mut cat, &Query::default(), 5, 8)? == (1..=23).collect::<Vec<_>>());
    Ok(())
}

#[test]
fn large_keyword_move_is_one_semantic_edit_and_cancelled_batches_never_continue() -> Result<()> {
    let (_temp, _root, photos, mut cat) = setup()?;
    photo(&photos.join("large.jpg"))?;
    let mut xml = String::from(
        r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="large" xmlns:lr="http://ns.adobe.com/lightroom/1.0/"><lr:hierarchicalSubject><rdf:Bag>"#,
    );
    for i in (0..1007).rev() {
        xml.push_str(&format!("<rdf:li>Old|Child{i:04}</rdf:li>"));
    }
    xml.push_str("<rdf:li>Unrelated|Keep</rdf:li></rdf:Bag></lr:hierarchicalSubject></rdf:Description></rdf:RDF>");
    fs::write(photos.join("large.xmp"), &xml)?;
    cat.import(&photos, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    apply(
        &mut cat,
        &asset.id,
        Operation::MoveKeyword {
            from: vec!["Old".into()],
            to: vec!["New".into()],
        },
    )?;
    let view = cat.metadata(&asset.id)?;
    let values = &view
        .fields
        .iter()
        .find(|f| f.name == "hierarchical_keywords")
        .unwrap()
        .value;
    let Some(photocatalog::xmp::Value::List(values)) = values else {
        anyhow::bail!("missing hierarchy");
    };
    ensure!(values.len() == 1008 && values.contains(&"Unrelated|Keep".into()));
    for i in 0..1007 {
        ensure!(values.contains(&format!("New|Child{i:04}")));
    }
    let job = cat.begin_organization_batch(Operation::Rating { value: 2 })?;
    cat.append_organization_batch(
        &job.id,
        &[BatchItem {
            asset_id: asset.id.clone(),
            expected_revision: view.revision,
        }],
    )?;
    cat.seal_organization_batch(&job.id)?;
    let cancelled = cat.cancel_organization_batch(&job.id)?;
    ensure!(cancelled.pending == 1 && cancelled.applied == 0 && cancelled.state == "cancelled");
    ensure!(cat.step_organization_batch(&job.id).is_err());
    ensure!(cat.metadata(&asset.id)?.revision == view.revision);
    ensure!(fs::read_to_string(photos.join("large.xmp"))? == xml);
    Ok(())
}

#[test]
fn unresolved_source_values_and_explicit_removal_do_not_reappear_in_filters() -> Result<()> {
    let (_temp, root, photos, mut cat) = setup()?;
    photo(&photos.join("one.jpg"))?;
    fs::write(photos.join("one.xmp"), packet(3))?;
    fs::write(photos.join("one.jpg.xmp"), packet(1))?;
    cat.import(&photos, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let row = cat.search(&Query::default(), None, 1, 1)?.rows.remove(0);
    ensure!(row.rating.is_none() && row.conflicts.contains(&"rating".into()));
    ensure!(
        all(
            &mut cat,
            &Query {
                rating: Some(3),
                ..Query::default()
            },
            1,
            1
        )?
        .is_empty()
    );
    let view = cat.metadata(&asset.id)?;
    let rating = view.fields.iter().find(|f| f.name == "rating").unwrap();
    let selected = rating
        .candidates
        .iter()
        .find(|v| v.value == photocatalog::xmp::Value::Text("3".into()))
        .unwrap()
        .model_id;
    cat.resolve_metadata(&asset.id, view.revision, "rating", selected)?;
    ensure!(
        all(
            &mut cat,
            &Query {
                rating: Some(3),
                ..Query::default()
            },
            1,
            1
        )? == vec![asset.sequence]
    );
    // Independent decoded fallback must not resurrect a deliberately removed field.
    db(&root)?.execute(
        "UPDATE assets SET metadata=json_set(metadata,'$.lens','fallback lens') WHERE id=?",
        [&asset.id],
    )?;
    finish_index(&mut cat)?;
    let revision = cat.metadata(&asset.id)?.revision;
    let set = cat.edit_metadata(
        &asset.id,
        revision,
        Some(selected),
        &[photocatalog::xmp::Edit::Set {
            namespace: "http://ns.adobe.com/exif/1.0/aux/".into(),
            path: "Lens".into(),
            value: "chosen lens".into(),
        }],
    )?;
    cat.edit_metadata(
        &asset.id,
        set.revision,
        Some(set.model_ids[0]),
        &[photocatalog::xmp::Edit::Remove {
            namespace: "http://ns.adobe.com/exif/1.0/aux/".into(),
            path: "Lens".into(),
        }],
    )?;
    ensure!(
        cat.search(&Query::default(), None, 1, 1)?.rows[0]
            .lens
            .is_empty()
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn unix_backslash_components_remain_literal_folder_and_filename_data() -> Result<()> {
    let (_temp, root, photos, mut cat) = setup()?;
    let folder = photos.join("literal\\").join("child");
    fs::create_dir_all(&folder)?;
    photo(&folder.join("back\\slash.jpg"))?;
    cat.import(&photos, None, |_| Ok(()))?;
    let row = cat.search(&Query::default(), None, 1, 1)?.rows.remove(0);
    ensure!(row.filename == "back\\slash.jpg");
    let locator: String = db(&root)?.query_row(
        "SELECT locator FROM organization_folders WHERE id=?",
        [row.folder.unwrap()],
        |r| r.get(0),
    )?;
    ensure!(serde_json::from_str::<NativePath>(&locator)? == NativePath::from_path(&folder));
    Ok(())
}

#[test]
fn nonpixel_edits_preserve_preview_authority_but_source_and_arbitrary_edits_invalidate()
-> Result<()> {
    let (_temp, _root, photos, mut cat) = setup()?;
    photo(&photos.join("one.jpg"))?;
    fs::write(photos.join("one.xmp"), packet(3))?;
    cat.import(&photos, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let queued = cat.render_identity(&asset.id)?;
    for operation in [
        Operation::Rating { value: 5 },
        Operation::Label {
            value: "red".into(),
        },
        Operation::Flag { value: Flag::Pick },
        Operation::AddKeyword {
            kind: KeywordKind::Flat,
            path: vec!["term".into()],
        },
        Operation::MoveKeyword {
            from: vec!["A".into()],
            to: vec!["B".into()],
        },
    ] {
        apply(&mut cat, &asset.id, operation)?;
        ensure!(cat.render_identity(&asset.id)?.generation == queued.generation);
        ensure!(cat.with_render_identity(&queued, || Ok(7))? == Some(7));
    }
    ensure!(cat.render_identity(&asset.id)?.metadata_revision > queued.metadata_revision);
    let view = cat.metadata(&asset.id)?;
    let mid = view
        .fields
        .iter()
        .find(|f| f.name == "rating")
        .unwrap()
        .selected_model
        .unwrap();
    cat.edit_metadata(
        &asset.id,
        view.revision,
        Some(mid),
        &[photocatalog::xmp::Edit::Set {
            namespace: "http://ns.adobe.com/tiff/1.0/".into(),
            path: "Orientation".into(),
            value: "6".into(),
        }],
    )?;
    ensure!(cat.with_render_identity(&queued, || Ok(7))?.is_none());
    let prior_source = cat.render_identity(&asset.id)?;
    fs::write(photos.join("one.xmp"), packet(1))?;
    cat.import(&photos, None, |_| Ok(()))?;
    ensure!(cat.with_render_identity(&prior_source, || Ok(7))?.is_none());
    let prior_choice = cat.render_identity(&asset.id)?;
    let view = cat.metadata(&asset.id)?;
    let rating = view.fields.iter().find(|f| f.name == "rating").unwrap();
    let source = rating
        .candidates
        .iter()
        .find(|c| c.source_kind == "sidecar")
        .unwrap()
        .model_id;
    cat.resolve_metadata(&asset.id, view.revision, "rating", source)?;
    ensure!(cat.with_render_identity(&prior_choice, || Ok(7))?.is_none());
    Ok(())
}
