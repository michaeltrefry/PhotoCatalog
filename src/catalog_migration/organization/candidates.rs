//! Imported observations never resolve source precedence. Scalar conflicts use
//! isolated field packets; keyword memberships accumulate only this import's
//! proven terms. Existing interactive edits and successful receipts are unchanged.
use super::*;
use crate::{
    catalog_metadata::{self, Prepared, Source},
    catalog_migration::{lookup::Lookup, walk::Walk},
    xmp::{self, Edit},
    xmp_packets::{
        Container, Inspection, Issue, Packet, ParseInput, SourceRevision, Status, Transformation,
    },
};
use rusqlite::Transaction;
use std::collections::BTreeSet;

const LIMIT: usize = 8 * 1024 * 1024;
const LR: &str = "http://ns.adobe.com/lightroom/1.0/";

pub(super) struct Candidate {
    source: Source,
    prepared: Prepared,
    field: String,
    scalar_conflict: bool,
    value_retained_only: bool,
    prefix: Vec<Prefix>,
}

struct Prefix {
    request: Projection,
    result: ProjectionResult,
    keyword_id: i64,
    kind: KeywordKind,
    path: Vec<String>,
}

pub(super) fn conflicted(db: &Connection, image: &str, field: &str) -> Result<bool> {
    Ok(db.query_row("SELECT COALESCE((SELECT conflicted FROM metadata_effective WHERE asset_id=?1 AND field=?2),0)",
        params![image, field], |r| r.get(0))?)
}

fn locator(request: &Projection, image: &SourceRecord, field: &str) -> Result<Vec<u8>> {
    let identity = encoded(&(
        request.import_source.as_str(),
        image.source.identity()?,
        field,
    ))?;
    Ok(format!(
        "lightroom-organization-candidate-v1:{}",
        blake3::hash(&identity).to_hex()
    )
    .into_bytes())
}

pub(super) fn inspection(bytes: Vec<u8>, limited: bool) -> Inspection {
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let status = if limited {
        Status::ResourceLimit
    } else {
        Status::Complete
    };
    Inspection {
        revision: SourceRevision {
            length: bytes.len() as u64,
            blake3: hash.clone(),
            modified_unix_ns: None,
        },
        status,
        issues: if limited {
            vec![Issue { status, offset: None, message: "Imported keyword accumulator reached its bound; this partial set is not authoritative. Complete source relations remain retained.".into() }]
        } else {
            vec![]
        },
        packets: vec![Packet {
            container: Container::CatalogXmp,
            bytes: bytes.clone(),
            blake3: hash.clone(),
            ranges: vec![],
            group: "imported-organization-candidate".into(),
            attributes: BTreeMap::new(),
        }],
        parse_inputs: vec![ParseInput {
            bytes,
            blake3: hash,
            packet_indices: vec![0],
            transformation: Transformation::Identity,
            group: "imported-organization-candidate".into(),
        }],
    }
}

/// Recover only successful old Image receipts, through the exact retained reverse
/// reference index. Never seed from an external/effective/user-authored packet.
/// Bounds fail closed before any new observation; unavailable keys prove nothing.
fn seed(
    catalog: &Catalog,
    source: &MigrationSource,
    request: &Projection,
    image_ref: &SourceRecord,
    kind: KeywordKind,
    evidence: &mut Evidence,
) -> Result<(BTreeSet<String>, Vec<Prefix>, bool)> {
    let walk = Walk::new(catalog, source, &image_ref.source.capture_revision)?;
    let sid = walk.source_id(image_ref)?;
    let entity = walk
        .singleton(&Lookup::EntitiesBySource(sid))?
        .context("keyword image entity unavailable")?;
    let record = catalog.migration_lookup_record(entity)?;
    let key = field(&catalog.db, entity, &record, "local_key")?;
    let page = catalog.migration_lookup(
        source.binding_blake3(),
        &image_ref.source.capture_revision,
        &Lookup::ReferencesByTargetKey {
            target_table: "Adobe_images".into(),
            target_key: key.clone(),
        },
        None,
        1,
    )?;
    ensure!(
        page.coverage_complete && page.keys_complete,
        "keyword prefix recovery exceeds bounded indexed coverage; explicit reconciliation required"
    );
    let mut terms = BTreeSet::new();
    let mut seeded = Vec::new();
    let mut seen = BTreeSet::new();
    // Apply the membership-table predicate before the cardinality bound; an
    // image can have thousands of unrelated incoming develop/history references.
    let rows = catalog.db.prepare("SELECT r.record FROM migration_record_lookup f INDEXED BY migration_lookup_target_key JOIN migration_record_lookup r INDEXED BY migration_lookup_source ON r.input=f.input AND r.revision=f.revision AND r.collection=3 AND r.source_id=f.source_id WHERE f.input=?1 AND f.revision=?2 AND f.collection=5 AND f.target_table='Adobe_images' AND f.target_key=?3 AND f.field='image' AND r.table_name='AgLibraryKeywordImage' ORDER BY f.record LIMIT 10001")?
        .query_map(params![source.binding_blake3(), image_ref.source.capture_revision, key], |r| r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.len() > 10_000 {
        return Ok((BTreeSet::new(), vec![], true));
    }
    let mut scanned_bytes = 0usize;
    for row in rows {
        let length: i64 = catalog.db.query_row(
            "SELECT raw_length FROM migration_retained_records WHERE sequence=?1 AND complete=1",
            [row],
            |r| r.get(0),
        )?;
        scanned_bytes = scanned_bytes.saturating_add(usize::try_from(length)?);
        if scanned_bytes > MAX_BYTES {
            return Ok((BTreeSet::new(), vec![], true));
        }
        let origin = walk.source_record(row)?;
        if origin.source.table != "AgLibraryKeywordImage" {
            continue;
        }
        let identity = origin.source.identity()?;
        if !seen.insert(identity.clone()) {
            continue;
        }
        let stored: Option<(String, String)> = catalog.db.query_row(
            "SELECT proof,result FROM migration_organization WHERE source_identity=?1 AND slot='keyword_membership' AND owner=?2",
            params![identity, request.import_source], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((proof, result)) = stored else {
            continue;
        };
        ensure!(
            proof.len() <= MAX_BYTES && result.len() <= 65536,
            "keyword prefix proof bound"
        );
        let value: serde_json::Value = serde_json::from_str(&proof)?;
        let prior: Projection = serde_json::from_value(
            value
                .get("request")
                .context("keyword prefix request missing")?
                .clone(),
        )?;
        ensure!(
            prior.origin.source == origin.source && prior.import_source == request.import_source,
            "keyword prefix source/owner differs"
        );
        let Decision::KeywordMembership {
            image,
            keyword: term,
        } = &prior.decision
        else {
            anyhow::bail!("keyword prefix decision differs");
        };
        ensure!(
            image.target.source == image_ref.source,
            "keyword prefix image differs"
        );
        let digest = input_digest(&prior)?;
        let actual =
            existing(&catalog.db, &prior, &digest)?.context("keyword prefix receipt missing")?;
        ensure!(
            actual == serde_json::from_str::<ProjectionResult>(&result)?,
            "keyword prefix receipt differs"
        );
        let (keyword_id, prior_kind, path) =
            keyword(&catalog.db, &request.import_source, &term.target)?;
        if kind != prior_kind {
            continue;
        }
        let mut ids = BTreeSet::from([prior.origin.retained_record]);
        for link in prior.decision.links() {
            ids.extend([
                link.reference_record,
                link.target_entity_record,
                link.target.retained_record,
            ]);
        }
        let mut additional = 0usize;
        let mut additional_bytes = 0usize;
        for id in ids {
            if !evidence.records.contains_key(&id) {
                additional += 1;
                let size:i64 = catalog.db.query_row("SELECT raw_length FROM migration_retained_records WHERE sequence=?1 AND complete=1",[id],|r|r.get(0))?;
                additional_bytes = additional_bytes.saturating_add(usize::try_from(size)?);
            }
        }
        if evidence.records.len() + additional > MAX_RECORDS
            || additional_bytes > MAX_BYTES - evidence.bytes
        {
            return Ok((BTreeSet::new(), vec![], true));
        }
        evidence.source(&catalog.db, &prior.origin)?;
        verify_unique_link(&catalog.db, evidence, &prior.origin, image, source)?;
        verify_unique_link(&catalog.db, evidence, &prior.origin, term, source)?;
        let expected = super::image(&catalog.db, &request.import_source, image_ref)?;
        ensure!(
            matches!(&actual.target, NativeTarget::Image { key, .. } if *key == expected.key),
            "keyword prefix is not an old native Image receipt"
        );
        if kind == prior_kind {
            terms.insert(if kind == KeywordKind::Flat {
                path[0].clone()
            } else {
                path.join("|")
            });
            seeded.push(Prefix {
                request: prior,
                result: actual,
                keyword_id,
                kind: prior_kind,
                path,
            });
        }
    }
    Ok((terms, seeded, false))
}

pub(super) fn prepare(
    catalog: &Catalog,
    source: Option<&MigrationSource>,
    request: &Projection,
    image_ref: &SourceRecord,
    expected: &ImageMetadataIdentity,
    operation: &Operation,
    evidence: &mut Evidence,
) -> Result<Option<Candidate>> {
    let (field_name, namespace, property) = match operation {
        Operation::Rating { .. } => ("rating", xmp::XMP, "Rating"),
        Operation::Label { .. } => ("label", xmp::XMP, "Label"),
        Operation::AddKeyword {
            kind: KeywordKind::Flat,
            ..
        } => ("keywords", xmp::DC, "subject"),
        Operation::AddKeyword { .. } => ("hierarchical_keywords", LR, "hierarchicalSubject"),
        _ => return Ok(None),
    };
    let scalar = !matches!(operation, Operation::AddKeyword { .. });
    if scalar && !conflicted(&catalog.db, &expected.image_id, field_name)? {
        return Ok(None);
    }
    let locator = locator(request, image_ref, field_name)?;
    let mut previous = None;
    let mut seeded = Vec::new();
    let mut limited = false;
    let empty = xmp::empty_packet()?;
    let bytes = match operation {
        Operation::Rating { value } => xmp::apply_organization_edits(
            &empty,
            &[Edit::Set {
                namespace: namespace.into(),
                path: property.into(),
                value: value.to_string(),
            }],
        )?,
        Operation::Label { value } => xmp::apply_organization_edits(
            &empty,
            &[Edit::Set {
                namespace: namespace.into(),
                path: property.into(),
                value: value.clone(),
            }],
        )?,
        Operation::AddKeyword { kind, path } => {
            let current: Option<(i64, i64, String, i64)> = catalog.db.query_row(
                "SELECT o.id,m.id,o.status,b.raw_length FROM image_metadata_sources s JOIN metadata_observations o ON o.id=s.current_observation JOIN metadata_models m ON m.observation_id=o.id JOIN metadata_blobs b ON b.hash=m.blob_hash WHERE s.asset_id=?1 AND s.kind='catalog' AND s.locator=?2 AND m.ordinal=0",
                params![expected.image_id, locator], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
            let (base, terms) = if let Some((observation, model, status, length)) = current {
                ensure!(
                    (0..=LIMIT as i64).contains(&length),
                    "keyword accumulator read bound"
                );
                previous = Some(observation);
                limited = status == "ResourceLimit";
                ensure!(
                    limited || status == "Complete",
                    "unexpected keyword accumulator status"
                );
                (
                    catalog.metadata_model(&expected.image_id, model)?,
                    BTreeSet::new(),
                )
            } else {
                let (terms, receipts, seed_limited) = seed(
                    catalog,
                    source.context("keyword accumulator seed requires sealed source")?,
                    request,
                    image_ref,
                    *kind,
                    evidence,
                )?;
                seeded = receipts;
                limited = seed_limited;
                (empty.clone(), terms)
            };
            let mut terms = terms;
            let parsed = xmp::parse(&base)?;
            for i in 1..=parsed.array_len(namespace, property) {
                terms.insert(
                    parsed
                        .array_item(namespace, property, i as i32)
                        .context("keyword accumulator item missing")?
                        .value,
                );
            }
            terms.insert(if *kind == KeywordKind::Flat {
                path[0].clone()
            } else {
                path.join("|")
            });
            // XML escaping is at most six bytes per input byte. Reserve structure
            // overhead per term before calling the native serializer.
            let bound = terms.iter().try_fold(empty.len(), |total, term| {
                total.checked_add(term.len().checked_mul(6)?.checked_add(128)?)
            });
            if limited || terms.len() > 10_000 || bound.is_none_or(|n| n > LIMIT) {
                limited = true;
                base
            } else {
                let edits = terms
                    .into_iter()
                    .map(|value| Edit::Append {
                        namespace: namespace.into(),
                        path: property.into(),
                        value,
                        ordered: false,
                    })
                    .collect::<Vec<_>>();
                xmp::apply_organization_edits(&empty, &edits)?
            }
        }
        _ => unreachable!(),
    };
    ensure!(bytes.len() <= LIMIT, "imported candidate serialized bound");
    let metadata_source = Source {
        kind: "catalog".into(),
        locator,
        display: "Imported Lightroom organization candidate".into(),
        ambiguous: false,
        provenance: serde_json::json!({"protocol":1,"owner":request.import_source,"image_source":image_ref.source,"field":field_name,"request_digest":input_digest(request)?,"origin":request.origin.source,"previous_observation":previous,"seeded_receipts":seeded.iter().map(|p|serde_json::json!({"source_identity":p.result.source_identity,"input_digest":p.result.input_digest})).collect::<Vec<_>>(),"value_retained_only":limited,"scope":"processed source terms; no precedence selected"}),
    };
    let prepared = Prepared::new(&inspection(bytes, limited), &metadata_source)?;
    Ok(Some(Candidate {
        source: metadata_source,
        prepared,
        field: field_name.into(),
        scalar_conflict: scalar,
        value_retained_only: limited,
        prefix: seeded,
    }))
}

impl Candidate {
    pub(super) fn commit(
        self,
        tx: &Transaction<'_>,
        expected: &ImageMetadataIdentity,
    ) -> Result<NativeTarget> {
        if self.scalar_conflict {
            ensure!(
                conflicted(tx, &expected.image_id, &self.field)?,
                "metadata conflict changed during candidate preparation; retry"
            );
        }
        for prefix in &self.prefix {
            ensure!(
                existing(tx, &prefix.request, &prefix.result.input_digest)?.as_ref()
                    == Some(&prefix.result),
                "keyword prefix receipt changed during preparation"
            );
            let Decision::KeywordMembership { keyword: term, .. } = &prefix.request.decision else {
                unreachable!()
            };
            ensure!(
                keyword(tx, &prefix.request.import_source, &term.target)?
                    == (prefix.keyword_id, prefix.kind, prefix.path.clone()),
                "keyword prefix dictionary endpoint changed during preparation"
            );
        }
        // Existing S4 observation semantics deliberately make a choice of an old
        // accumulator model stale; independent local choices remain effective.
        // No source choice is inserted, removed, or redirected here.
        let change = catalog_metadata::retain_prepared(
            tx,
            &expected.image_id,
            &self.source,
            &self.prepared,
            false,
        )?;
        ensure!(
            change.model_ids.len() == 1,
            "imported candidate requires one model"
        );
        Ok(NativeTarget::MetadataCandidate {
            key: expected.key.clone(),
            revision: change.revision,
            field: self.field.clone(),
            observation_id: change.observation_id,
            model_id: change.model_ids[0],
            conflicted: conflicted(tx, &expected.image_id, &self.field)?,
            value_retained_only: self.value_retained_only,
        })
    }
}
