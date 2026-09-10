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
    // Windows canonical temp roots can use extended spelling; folder keys use the
    // corresponding drive/UNC spelling. Ask the filesystem whether they identify
    // the actual relocated directory, rather than comparing alias bytes.
    let stored_folder = serde_json::from_str::<NativePath>(&stored)?.to_path()?;
    ensure!(stored_folder.canonicalize()? == new.canonicalize()?);
    let stored_original: String = db(&root)?.query_row(
        "SELECT native_path FROM storage_bindings WHERE asset_id=?",
        [&asset.id],
        |r| r.get(0),
    )?;
    let canonical_original = new.join("one.jpg").canonicalize()?;
    ensure!(
        serde_json::from_str::<NativePath>(&stored_original)?
            == NativePath::from_path(&canonical_original)
    );
    ensure!(cat.get(&asset.id)?.original_path == canonical_original.to_string_lossy());
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

#[test]
fn current_catalog_query_open_does_not_write_or_wait_for_an_existing_writer() -> Result<()> {
    let (_temp, root, cat) = synthetic(5)?;
    drop(cat);
    let main = root.join("catalog.sqlite3");
    let before = fs::read(&main)?;
    for _ in 0..3 {
        drop(Catalog::open(&root)?);
        ensure!(fs::read(&main)? == before);
    }
    let writer = db(&root)?;
    writer.execute_batch("BEGIN IMMEDIATE")?;
    let wal = root.join("catalog.sqlite3-wal");
    let before_wal = fs::read(&wal).unwrap_or_default();
    // The writer stays held throughout open and query; an accidental IMMEDIATE
    // initialization transaction would time out instead of completing this read.
    let mut reader = Catalog::open(&root)?;
    ensure!(reader.search(&Query::default(), None, 5, 5)?.rows.len() == 5);
    ensure!(fs::read(&main)? == before && fs::read(&wal).unwrap_or_default() == before_wal);
    drop(reader);
    writer.execute_batch("ROLLBACK")?;
    drop(writer);
    ensure!(fs::read(&main)? == before);
    Ok(())
}

#[test]
fn deferred_upgrade_repro_and_real_distinct_asset_writers_serialize_without_lost_data() -> Result<()>
{
    use std::sync::mpsc;
    use std::time::Duration;
    let (_temp, root, photos, mut cat) = setup()?;
    for name in ["one", "two"] {
        photo(&photos.join(format!("{name}.jpg")))?;
        fs::write(photos.join(format!("{name}.xmp")), packet(3))?;
    }
    cat.import(&photos, None, |_| Ok(()))?;
    let assets = cat.browse(0, 2)?;
    let first = assets[0].id.clone();
    let second = assets[1].id.clone();
    let old_revision = cat.metadata(&first)?.revision;
    // Force the historical failure deterministically: an older read snapshot
    // cannot become a writer after an independent asset commits in WAL mode.
    let stale = db(&root)?;
    stale.execute_batch("BEGIN DEFERRED")?;
    let _: i64 = stale.query_row(
        "SELECT revision FROM metadata_assets WHERE asset_id=?",
        [&first],
        |r| r.get(0),
    )?;
    apply(&mut cat, &second, Operation::Flag { value: Flag::Pick })?;
    let error = stale
        .execute(
            "UPDATE metadata_assets SET revision=revision+1 WHERE asset_id=?",
            [&first],
        )
        .unwrap_err();
    ensure!(
        matches!(error,rusqlite::Error::SqliteFailure(ref code,_) if code.extended_code==517),
        "expected SQLITE_BUSY_SNAPSHOT, got {error}"
    );
    stale.execute_batch("ROLLBACK")?;
    drop(stale);
    ensure!(cat.metadata(&first)?.revision == old_revision);
    let expected_second = cat.metadata(&second)?.revision;
    let mut other = Catalog::open(&root)?;
    let (go_tx, go_rx) = mpsc::sync_channel(0);
    let (started_tx, started_rx) = mpsc::sync_channel(0);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let other_id = second.clone();
    let worker = std::thread::spawn(move || {
        go_rx.recv().unwrap();
        started_tx.send(()).unwrap();
        let result =
            other.organize_asset(&other_id, expected_second, Operation::Rating { value: 5 });
        done_tx.send(result).unwrap();
    });
    let job = cat.begin_organization_batch(Operation::Flag {
        value: Flag::Reject,
    })?;
    cat.append_organization_batch(
        &job.id,
        &[BatchItem {
            asset_id: first.clone(),
            expected_revision: old_revision,
        }],
    )?;
    cat.seal_organization_batch(&job.id)?;
    let outcome = cat.step_organization_batch_with(&job.id, || {
        go_tx.send(())?;
        started_rx.recv()?;
        ensure!(
            matches!(
                done_rx.recv_timeout(Duration::from_millis(40)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "second writer did not wait for held authority"
        );
        Ok(())
    })?;
    let result = done_rx.recv_timeout(Duration::from_secs(5));
    worker.join().unwrap();
    ensure!(outcome.state == "complete", "first atomic writer failed");
    ensure!(result?? == expected_second + 1);
    ensure!(cat.metadata(&first)?.revision == old_revision + 1);
    let second_view = cat.metadata(&second)?;
    ensure!(
        second_view.revision == expected_second + 1
            && second_view.fields.iter().any(|f| f.name == "rating"
                && f.value == Some(photocatalog::xmp::Value::Text("5".into())))
    );
    ensure!(
        cat.search(
            &Query {
                flag: Some(Flag::Reject),
                ..Query::default()
            },
            None,
            10,
            10
        )?
        .rows
        .iter()
        .any(|r| r.asset_id == first)
    );
    for name in ["one", "two"] {
        ensure!(fs::read_to_string(photos.join(format!("{name}.xmp")))? == packet(3));
    }
    Ok(())
}

// Independent source text fixtures: both production projections are populated
// identically, while expected IDs below are specified without the SQL builder.
fn replace_search_text(root: &Path, texts: &[&str]) -> Result<()> {
    let mut conn = db(root)?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM organization_text", [])?;
    for (index, text) in texts.iter().enumerate() {
        let sequence = index as i64 + 1;
        tx.execute(
            "UPDATE organization_assets SET search_text=?1 WHERE sequence=?2",
            params![text, sequence],
        )?;
        tx.execute(
            "INSERT INTO organization_text(rowid,text) VALUES(?1,?2)",
            params![sequence, text],
        )?;
    }
    tx.execute("UPDATE organization_state SET epoch=epoch+1", [])?;
    tx.commit()?;
    Ok(())
}

#[test]
fn local_text_preserves_native_unicode_phrase_prefix_and_literal_syntax_all_orders() -> Result<()> {
    let (_temp, root, mut cat) = synthetic(12)?;
    replace_search_text(
        &root,
        &[
            "Café sunset",
            "CAFE\u{301} SUNSETS",
            "cafe sun",
            "pré-visualisation glacier",
            "pre visualization glacial",
            "pre something visualization glacier",
            "東京 山道",
            "東京 山岳",
            "россия Закат",
            "coöperate \"quoted\" alpha beta",
            "ab\"cd xy",
            "literal OR NEAR NOT",
        ],
    )?;
    let cases: &[(&str, &[i64])] = &[
        ("cafe suns", &[1, 2]),
        ("pre-visu glac", &[4, 5]),
        ("東京 山", &[7, 8]),
        ("РОСС за", &[9]),
        ("cooperate quo", &[10]),
        ("ab\"c", &[11]),
        ("OR", &[12]),
        ("\"quoted\"", &[10]),
        ("absent", &[]),
    ];
    for (text, expected_ids) in cases {
        for sort in [Sort::Sequence, Sort::Capture, Sort::Filename, Sort::Rating] {
            for direction in [Direction::Ascending, Direction::Descending] {
                for lens in [None, Some("lens0".to_string())] {
                    let query = Query {
                        text: Some((*text).into()),
                        lens: lens.clone(),
                        sort,
                        direction,
                        ..Query::default()
                    };
                    let mut expected: Vec<i64> = expected_ids
                        .iter()
                        .copied()
                        .filter(|i| lens.is_none() || i % 4 == 0)
                        .collect();
                    expected.sort_by_key(|i| match sort {
                        Sort::Capture => (i % 28, *i),
                        Sort::Rating => (i % 6, *i),
                        _ => (*i, *i),
                    });
                    if direction == Direction::Descending {
                        expected.reverse();
                    }
                    ensure!(
                        all(&mut cat, &query, 2, 3)? == expected,
                        "text={text:?} sort={sort:?} direction={direction:?} lens={lens:?}"
                    );
                    let first = cat.search(&query, None, 2, 3)?;
                    ensure!(first.sorts == 0);
                    let plans = cat.explain_search(&query, None, 3)?;
                    ensure!(
                        !plans
                            .iter()
                            .any(|p| p.contains("CORRELATED SCALAR SUBQUERY"))
                    );
                    if sort == Sort::Sequence {
                        ensure!(first.text_work.indexed_rows == 0 && first.text_work.vm_steps == 0);
                        ensure!(plans.iter().filter(|p| p.contains("VIRTUAL TABLE")).count() == 1);
                    } else {
                        ensure!(!plans.iter().any(|p| p.contains("VIRTUAL TABLE")));
                        ensure!(first.text_work.vm_steps > 0);
                    }
                }
            }
        }
    }
    // A membership driver must use local text even when sorting by sequence.
    let keyword: i64 = db(&root)?.query_row(
        "SELECT id FROM organization_keywords WHERE kind='hierarchical' AND name='Group4'",
        [],
        |r| r.get(0),
    )?;
    let query = Query {
        text: Some("pre-visu".into()),
        keyword: Some(keyword),
        ..Query::default()
    };
    ensure!(all(&mut cat, &query, 2, 3)? == vec![4]);
    ensure!(cat.search(&query, None, 2, 3)?.text_work.indexed_rows > 0);
    Ok(())
}

#[test]
fn local_text_byte_admission_continues_without_skips_and_oversize_retry_is_explicit() -> Result<()>
{
    use photocatalog::organization_search::TextLimits;
    let (_temp, root, mut cat) = synthetic(31)?;
    let text = "cafe sunset a moderately sized document";
    replace_search_text(&root, &vec![text; 31])?;
    let limits = TextLimits {
        document_bytes: text.len(),
        page_bytes: text.len() * 2,
    };
    for direction in [Direction::Ascending, Direction::Descending] {
        let query = Query {
            text: Some("cafe suns".into()),
            sort: Sort::Filename,
            direction,
            ..Query::default()
        };
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let page = cat.search_with_text_limits(&query, cursor.as_ref(), 7, 9, limits)?;
            ensure!(page.scanned <= 2 && page.text_work.indexed_rows == page.scanned);
            ensure!(page.text_work.indexed_bytes == page.scanned * text.len());
            ensure!(page.text_work.indexed_bytes <= limits.page_bytes);
            ensure!(page.vm_steps >= page.text_work.vm_steps && page.text_work.vm_steps > 0);
            if page.text_work.admission_limited {
                ensure!(!page.page_complete && page.has_more && !page.exhausted);
                ensure!(page.text_work.candidate_rows_read == page.scanned + 1);
            }
            ids.extend(page.rows.iter().map(|r| r.sequence));
            if page.exhausted {
                break;
            }
            ensure!(page.next.is_some());
            cursor = page.next;
        }
        let mut expected: Vec<i64> = (1..=31).collect();
        if direction == Direction::Descending {
            expected.reverse();
        }
        ensure!(ids == expected);
        let error = cat
            .search_with_text_limits(
                &query,
                None,
                2,
                3,
                TextLimits {
                    document_bytes: text.len() - 1,
                    ..limits
                },
            )
            .unwrap_err();
        ensure!(error.to_string().contains("retry with larger TextLimits"));
        // A failed call cannot leave stale TEMP matches in the next call.
        let page = cat.search_with_text_limits(&query, None, 2, 3, limits)?;
        ensure!(page.rows.iter().map(|r| r.sequence).collect::<Vec<_>>() == expected[..2]);
    }
    let query = Query {
        text: Some("notpresent".into()),
        sort: Sort::Filename,
        ..Query::default()
    };
    let partial = cat.search_with_text_limits(&query, None, 2, 3, limits)?;
    ensure!(
        partial.rows.is_empty()
            && !partial.page_complete
            && !partial.exhausted
            && partial.next.is_some()
    );
    ensure!(partial.text_work.admission_limited && partial.scanned == 2);
    Ok(())
}

#[test]
fn local_text_snapshot_temp_writes_preserve_main_and_do_not_upgrade_writer() -> Result<()> {
    use photocatalog::organization_search::TextLimits;
    let (_temp, root, mut cat) = synthetic(19)?;
    replace_search_text(&root, &vec!["café sunset"; 19])?;
    let query = Query {
        text: Some("cafe sun".into()),
        sort: Sort::Filename,
        ..Query::default()
    };
    let limits = TextLimits {
        document_bytes: 64,
        page_bytes: 64,
    };
    let mut session = cat.search_session_with_text_limits(query.clone(), 30, limits)?;
    let first = session.next_page(2, 3)?;
    let serial = cat.search(&query, None, 2, 3)?.next.unwrap();
    let writer = db(&root)?;
    writer.execute_batch("BEGIN IMMEDIATE")?;
    let image = || -> Result<Vec<(String, Vec<u8>)>> {
        let mut data = Vec::new();
        for name in ["catalog.sqlite3", "catalog.sqlite3-wal"] {
            let path = root.join(name);
            if path.exists() {
                data.push((name.into(), fs::read(path)?));
            }
        }
        Ok(data)
    };
    let before = image()?;
    // Both ordinary pages and the read-only-main snapshot may write TEMP while
    // another connection already owns main's writer reservation.
    ensure!(cat.search(&query, None, 2, 3)?.rows.len() == 2);
    let second = session.next_page(2, 3)?;
    ensure!(before == image()?);
    writer.execute_batch("ROLLBACK")?;
    drop(writer);
    replace_search_text(&root, &vec!["unrelated mountain"; 19])?;
    ensure!(cat.search(&query, Some(&serial), 2, 3).is_err());
    ensure!(all(&mut cat, &query, 2, 3)?.is_empty());
    let mut ids: Vec<_> = first
        .rows
        .into_iter()
        .chain(second.rows)
        .map(|r| r.sequence)
        .collect();
    loop {
        let page = session.next_page(2, 3)?;
        ids.extend(page.rows.iter().map(|r| r.sequence));
        if page.exhausted {
            break;
        }
    }
    ensure!(ids == (1..=19).collect::<Vec<_>>());
    session.close()?;
    // A new snapshot sees the new authoritative text.
    let mut fresh = cat.search_session(query, 30)?;
    let mut found = 0;
    loop {
        let p = fresh.next_page(2, 3)?;
        found += p.rows.len();
        if p.exhausted {
            break;
        }
    }
    ensure!(found == 0);
    fresh.close()?;
    Ok(())
}

#[test]
fn local_text_work_is_candidate_bounded_as_unrelated_postings_grow() -> Result<()> {
    let mut proofs = Vec::new();
    for count in [64, 640] {
        let (_temp, root, mut cat) = synthetic(count)?;
        replace_search_text(&root, &vec!["café sunset"; count as usize])?;
        let query = Query {
            text: Some("cafe suns".into()),
            sort: Sort::Filename,
            ..Query::default()
        };
        let page = cat.search(&query, None, 9, 17)?;
        ensure!(
            page.rows.iter().map(|r| r.sequence).collect::<Vec<_>>() == (1..=9).collect::<Vec<_>>()
        );
        ensure!(page.scanned == 9 && page.text_work.candidate_rows_read == 9);
        ensure!(page.text_work.indexed_rows == 9 && page.text_work.batches == 1 && page.sorts == 0);
        proofs.push((page.text_work.indexed_bytes, page.text_work.vm_steps));
    }
    ensure!(proofs[0] == proofs[1]);
    // This proves bounded staging on small inputs, not a scale latency or RSS gate.
    Ok(())
}

#[test]
fn capture_cursor_seeks_bound_ties_and_lens_dates_without_prefix_scans() -> Result<()> {
    let (_temp, root, mut cat) = synthetic(5600)?;
    for direction in [Direction::Ascending, Direction::Descending] {
        for lens in [None, Some("lens0".to_string())] {
            let query = Query {
                sort: Sort::Capture,
                direction,
                lens: lens.clone(),
                date_from: Some("2024-01-05".into()),
                date_until: Some("2024-01-20".into()),
                camera: Some("camera1".into()),
                ..Query::default()
            };
            let mut expected: Vec<_> = (1..=5600)
                .filter(|i| {
                    i % 3 == 1 && (4..19).contains(&(i % 28)) && (lens.is_none() || i % 4 == 0)
                })
                .collect();
            expected.sort_by_key(|i| (i % 28, *i));
            if direction == Direction::Descending {
                expected.reverse();
            }
            ensure!(all(&mut cat, &query, 13, 31)? == expected);
            let mut cursor = cat.search(&query, None, 13, 31)?.next;
            for _ in 0..4 {
                let page = cat.search(&query, cursor.as_ref(), 13, 31)?;
                ensure!(
                    page.vm_steps < 12000 && page.sorts == 0,
                    "prefix scan leaked: {} steps",
                    page.vm_steps
                );
                cursor = page.next;
                if page.exhausted {
                    break;
                }
            }
        }
    }
    // Day 26..28 have residues 25..27 modulo28, none divisible by4.
    // A lens-leading capture index must prove this true empty tail without
    // visiting every asset in those dates; the source data/formula is unchanged.
    let query = Query {
        sort: Sort::Capture,
        text: Some("blue sunset".into()),
        lens: Some("lens0".into()),
        date_from: Some("2024-01-26".into()),
        ..Query::default()
    };
    let page = cat.search(&query, None, 200, 4096)?;
    ensure!(
        page.rows.is_empty()
            && page.exhausted
            && page.scanned == 0
            && page.vm_steps < 1000
            && page.sorts == 0
    );
    ensure!(
        cat.explain_search(&query, None, 4096)?
            .iter()
            .any(|p| p.contains("organization_lens_capture"))
    );
    // Keep the table shape and exact logical data; only the additive index exists.
    ensure!(
        db(&root)?.query_row("SELECT COUNT(*) FROM organization_assets", [], |r| r
            .get::<_, i64>(0))?
            == 5600
    );
    Ok(())
}

#[test]
fn schema_four_to_current_preserves_old_rows_adds_empty_edit_tables_and_keeps_readonly_queries()
-> Result<()> {
    let (_temp, root, cat) = synthetic(113)?;
    drop(cat);
    let conn = db(&root)?;
    conn.execute_batch("PRAGMA foreign_keys=OFF; DROP INDEX storage_export_path; DROP INDEX storage_export_object; DROP TABLE photo_export_items; DROP TABLE photo_export_jobs; DROP TABLE photo_export_blobs; DROP TABLE edit_copy_items; DROP TABLE edit_copy_jobs; DROP TABLE edit_changes; DROP TABLE edit_redo_nodes; DROP TABLE edit_recipe_nodes; DROP TABLE edit_variants; PRAGMA foreign_keys=ON; DROP INDEX organization_lens_capture; PRAGMA user_version=4; PRAGMA wal_checkpoint(TRUNCATE)")?;
    fn contents(conn: &Connection) -> Result<Vec<(String, Vec<Vec<String>>)>> {
        let tables=conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name!='sqlite_stat1' ORDER BY name")?
            .query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut result = Vec::new();
        for table in tables {
            let quoted = table.replace('"', "\"\"");
            let mut stmt = conn.prepare(&format!("SELECT * FROM \"{quoted}\""))?;
            let columns = stmt.column_count();
            let mut rows = stmt
                .query_map([], |r| {
                    (0..columns)
                        .map(|i| r.get_ref(i).map(|v| format!("{v:?}")))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.sort();
            result.push((table, rows));
        }
        Ok(result)
    }
    let before = contents(&conn)?;
    drop(conn);
    let mut cat = Catalog::open(&root)?;
    let conn = db(&root)?;
    let mut after = contents(&conn)?;
    let added: Vec<_> = after
        .iter()
        .filter(|(name, _)| name.starts_with("edit_"))
        .collect();
    ensure!(added.len() == 6 && added.iter().all(|(_, rows)| rows.is_empty()));
    after.retain(|(name, _)| !name.starts_with("edit_"));
    ensure!(after == before);
    ensure!(
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?
            == photocatalog::CURRENT_SCHEMA_VERSION
    );
    let index: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE name='organization_lens_capture'",
        [],
        |r| r.get(0),
    )?;
    ensure!(index.contains("(lens,capture,sequence)"));
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); BEGIN IMMEDIATE")?;
    let main = fs::read(root.join("catalog.sqlite3"))?;
    let wal = fs::read(root.join("catalog.sqlite3-wal"))?;
    drop(Catalog::open(&root)?);
    let query = Query {
        sort: Sort::Capture,
        lens: Some("lens0".into()),
        text: Some("blue sunset".into()),
        ..Query::default()
    };
    ensure!(!cat.search(&query, None, 3, 10)?.rows.is_empty());
    let mut session = cat.search_session(query, 30)?;
    ensure!(!session.next_page(3, 10)?.rows.is_empty());
    session.close()?;
    ensure!(
        fs::read(root.join("catalog.sqlite3"))? == main
            && fs::read(root.join("catalog.sqlite3-wal"))? == wal
    );
    conn.execute_batch("ROLLBACK")?;
    Ok(())
}

#[test]
fn failed_schema_six_upgrade_rolls_back_all_added_tables_and_marker() -> Result<()> {
    let (_temp, root, catalog) = synthetic(3)?;
    drop(catalog);
    let conn = db(&root)?;
    conn.execute_batch("PRAGMA foreign_keys=OFF; DROP INDEX storage_export_path; DROP INDEX storage_export_object; DROP TABLE photo_export_items; DROP TABLE photo_export_jobs; DROP TABLE photo_export_blobs; DROP TABLE edit_copy_items; DROP TABLE edit_copy_jobs; DROP TABLE edit_changes; DROP TABLE edit_redo_nodes; DROP TABLE edit_recipe_nodes; DROP TABLE edit_variants; PRAGMA user_version=5; CREATE TABLE edit_copy_jobs(unexpected TEXT); PRAGMA wal_checkpoint(TRUNCATE)")?;
    fn schema(conn: &Connection) -> Result<Vec<(String, String, Option<String>)>> {
        Ok(conn
            .prepare("SELECT type,name,sql FROM sqlite_schema ORDER BY type,name")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }
    let before = schema(&conn)?;
    let assets: i64 = conn.query_row("SELECT count(*) FROM assets", [], |row| row.get(0))?;
    drop(conn);
    ensure!(Catalog::open(&root).is_err());
    let conn = db(&root)?;
    ensure!(
        schema(&conn)? == before,
        "failed migration left partial schema"
    );
    ensure!(conn.query_row::<i64, _, _>("PRAGMA user_version", [], |row| row.get(0))? == 5);
    ensure!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM assets", [], |row| row.get(0))? == assets
    );
    Ok(())
}
