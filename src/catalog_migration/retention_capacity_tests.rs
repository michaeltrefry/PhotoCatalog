use super::*;
use crate::{
    capacity_probes as probe,
    lightroom::{
        capture::Manifest,
        migration_source::{self as source, tests::Fixture},
        plan::Cell,
    },
};

// Deliberately violates the remote Page byte admission to exercise the core's
// defensive batch break. This is not advertised as a valid remote payload.
struct OversizedPage<'a>(&'a MigrationSource);
impl MigrationRead for OversizedPage<'_> {
    fn seal(&self) -> &source::InputSeal {
        self.0.seal()
    }
    fn binding_blake3(&self) -> &str {
        self.0.binding_blake3()
    }
    fn max_chunk_bytes(&self) -> usize {
        self.0.max_chunk_bytes()
    }
    fn capture_manifest(&self, r: &str) -> Result<Manifest> {
        self.0.capture_manifest(r)
    }
    fn stable_source(&self, r: &str, s: &str) -> Result<source::StableSource> {
        self.0.stable_source(r, s)
    }
    fn origin_packet_roster(&self, r: &str, s: &str, o: &str) -> Result<Vec<i64>> {
        self.0.origin_packet_roster(r, s, o)
    }
    fn read_chunk(&self, r: &source::ByteRef, o: u64, l: usize) -> Result<Vec<u8>> {
        self.0.read_chunk(r, o, l)
    }
    fn count(&self, r: &str, c: Collection) -> Result<u64> {
        self.0.count(r, c)
    }
    fn resolve(&self, r: &str, s: &str, f: &str, t: &str) -> Result<source::Resolution> {
        self.0.resolve(r, s, f, t)
    }
    fn image_links(&self, r: &str, s: &str) -> Result<source::ImageLinks> {
        self.0.image_links(r, s)
    }
    fn page(
        &self,
        r: &str,
        c: Collection,
        after: Option<&Cursor>,
        limit: usize,
    ) -> Result<source::Page> {
        if c != Collection::Rows {
            return self.0.page(r, c, after, limit);
        }
        let last = match after.and_then(|a| a.after.first()) {
            Some(Cell::Integer(n)) => *n,
            None => 0,
            _ => anyhow::bail!("probe cursor"),
        };
        let records: Vec<_> = ((last + 1)..=4)
            .take(limit)
            .map(|n| EvidenceRecord {
                revision: r.into(),
                collection: c,
                rowid: n,
                key: vec![Cell::Integer(n)],
                fields: std::collections::BTreeMap::from([
                    (
                        "source_id".into(),
                        Field::Inline(Cell::Text(format!("probe-{n}").into_bytes())),
                    ),
                    (
                        "table_name".into(),
                        Field::Inline(Cell::Text(b"unknown_plugin".to_vec())),
                    ),
                    (
                        "cells_json".into(),
                        Field::Inline(Cell::Text(vec![65; 1_100_000])),
                    ),
                    ("key_json".into(), Field::Inline(Cell::Text(b"[]".to_vec()))),
                ]),
            })
            .collect();
        let next = records.last().map(|v| Cursor {
            seal: self.binding_blake3().into(),
            revision: r.into(),
            collection: c,
            after: v.key.clone(),
        });
        Ok(source::Page {
            records,
            next,
            exhausted: true,
        })
    }
}

#[test]
fn capacity_retention_admitted_page_and_defensive_break_resume() -> Result<()> {
    let baseline = probe::begin();
    // Real Source page budget, external-field stop and exact restart proof.
    super::tests::batches_stop_before_uncustodied_bytes_and_resume_without_skips()?;
    let mut fixture = Fixture::new();
    let approval = b"capacity defensive page fixture";
    fixture.seal.approval.document_blake3 = blake3::hash(approval).to_hex().to_string();
    let source = fixture.open();
    let remote_page = source.page(fixture.revision(), Collection::Rows, None, 64)?;
    let page_bytes = serde_json::to_vec(&remote_page)?.len();
    let record_sum = remote_page
        .records
        .iter()
        .map(serde_json::to_vec)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .iter()
        .map(Vec::len)
        .sum::<usize>();
    assert!(record_sum <= page_bytes && page_bytes <= 8 * 1024 * 1024);
    let oversized = OversizedPage(&source);
    let defense_page = oversized.page(fixture.revision(), Collection::Rows, None, 64)?;
    assert!(serde_json::to_vec(&defense_page)?.len() > 8 * 1024 * 1024);
    drop(defense_page);
    let temp = tempfile::tempdir()?;
    let mut catalog = Catalog::open(temp.path().join("destination"))?;
    catalog.begin_migration_retention_reader(&oversized, approval)?;
    let initial_breaks = probe::visits(probe::STAGED_BREAK);
    let mut complete = false;
    for _ in 0..100 {
        if catalog
            .step_migration_retention_reader(&oversized)?
            .complete
        {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert!(probe::visits(probe::STAGED_BREAK) > initial_breaks);
    let collection_index = COLLECTIONS
        .iter()
        .position(|c| *c == Collection::Rows)
        .expect("Rows is retained");
    let stored: i64 = catalog.db.query_row(
        "SELECT count(*) FROM migration_retained_records WHERE input=?1 AND revision=?2 AND collection=?3 AND complete=1",
        params![source.binding_blake3(), fixture.revision(), i64::try_from(collection_index)?],
        |row| row.get(0),
    )?;
    assert_eq!(stored, 4);
    // Saved-record reads enforce the same aggregate byte ceiling. The fourth
    // large record is retrieved by its durable sequence cursor, not one page.
    let rows = catalog.retained_migration_records(
        source.binding_blake3(),
        fixture.revision(),
        Collection::Rows,
        0,
        10,
    )?;
    assert_eq!(rows.len(), 3);
    for (index, (_, record)) in rows.iter().enumerate() {
        assert_eq!(record.key, vec![Cell::Integer(index as i64 + 1)]);
    }
    assert!(rows.windows(2).all(|pair| pair[0].0 < pair[1].0));
    let after = rows.last().expect("first page").0;
    drop(rows);
    let last = catalog.retained_migration_records(
        source.binding_blake3(),
        fixture.revision(),
        Collection::Rows,
        after,
        10,
    )?;
    assert_eq!(last.len(), 1);
    assert!(last[0].0 > after);
    assert_eq!(last[0].1.key, vec![Cell::Integer(4)]);
    assert!(
        catalog
            .retained_migration_records(
                source.binding_blake3(),
                fixture.revision(),
                Collection::Rows,
                last[0].0,
                10,
            )?
            .is_empty()
    );
    println!(
        "CAPACITY_RETENTION admitted_page={page_bytes} admitted_record_sum={record_sum} defensive_records=4 exact_resume=true"
    );
    probe::report("retention", baseline);
    Ok(())
}
