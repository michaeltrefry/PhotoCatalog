//! Typed interpretation of the selected schema's organization rows. Native
//! projections and compatibility classifications keep the same source receipts.
use super::{
    importer::{KeywordOverlap, Outcome, Policy, RowResult, Stage, retained},
    organization::{
        self, Compatibility, Decision, DictionaryDecision, NativeTarget, Projection,
        ProjectionResult, SourceRecord,
    },
    walk::{LinkResolution, Walk},
};
use crate::{
    Catalog,
    lightroom::{
        migration_source::{Field, MigrationSource},
        plan::Cell,
    },
    organization::{Flag, KeywordKind},
};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};
const ADAPTER: &str = "lightroom-organization-columns-v2";
const LIMIT: usize = 8 * 1024 * 1024;
// A retained Cells/Columns descriptor may address arbitrarily large custody.
// Only interpretation is optional: source identity/schema errors still fail.
fn columns(
    catalog: &Catalog,
    walk: &Walk<'_>,
    origin: &SourceRecord,
) -> Result<Option<BTreeMap<String, Cell>>> {
    walk.source_id(origin)?;
    let table = walk.schema(&origin.source.table)?;
    let mut proof = organization::Evidence::default();
    super::images::table_proof(&catalog.db, &mut proof, origin, table)?;
    for (sequence, name) in [
        (origin.retained_record, "cells_json"),
        (table, "columns_json"),
    ] {
        let record = proof.record(&catalog.db, sequence)?;
        let fits = match record.fields.get(name) {
            Some(Field::Inline(Cell::Text(v))) => v.len() <= LIMIT,
            Some(Field::Bytes(v)) => v.text && v.bytes <= LIMIT as u64,
            _ => false,
        };
        if !fits {
            return Ok(None);
        }
    }
    Ok(Some(walk.columns(origin)?))
}
fn relation_description(value: LinkResolution) -> String {
    match value {
        LinkResolution::Missing => "missing".into(),
        LinkResolution::Ambiguous => "ambiguous".into(),
        LinkResolution::Unavailable { reason } => reason,
        LinkResolution::Unique(_) => "unique".into(),
    }
}
fn native_dictionary(value: Option<ProjectionResult>, collection: bool) -> bool {
    matches!(
        (value.map(|r| r.target), collection),
        (Some(NativeTarget::Collection { .. }), true) | (Some(NativeTarget::Keyword { .. }), false)
    )
}
fn name(value: &Cell) -> Option<String> {
    text(value).filter(|v| !v.trim().is_empty() && v.len() <= 1024 && !v.contains('\0'))
}
fn unsupported_row(
    catalog: &mut Catalog,
    source: &MigrationSource,
    policy: &Policy,
    origin: &SourceRecord,
    construct: &str,
) -> Result<RowResult> {
    preserve(
        catalog,
        source,
        policy,
        origin,
        construct,
        Compatibility::Unsupported,
        "Source columns exceed interpretation bounds or have unsupported types; exact selected raw row remains retained",
    )
}
fn behavior(
    catalog: &mut Catalog,
    source: &MigrationSource,
    policy: &Policy,
    origin: &SourceRecord,
) -> Result<ProjectionResult> {
    project_one(catalog,source,policy,origin,Decision::Retain {
        construct: if origin.source.table=="AgLibraryCollection" {"collection_behavior"} else {"keyword_behavior"}.into(),
        compatibility:Compatibility::Unsupported,
        detail:if origin.source.table=="AgLibraryCollection" {
            "Native container is a static membership snapshot; exact creationId, systemOnly, genealogy and source ordering are retained. Adobe smart rules, group restrictions and system behavior are not executed or equivalent."
        } else { "Native hierarchical keyword preserves its exact name/path. Source keywordType/person and export/include flags remain retained; Adobe person and export behavior are not asserted equivalent." }.into(),
        unresolved:None,
    })
}

fn integer(value: &Cell) -> Option<i64> {
    match value {
        Cell::Integer(v) => Some(*v),
        Cell::RealBits(v) => {
            let f = f64::from_bits(*v);
            if f.is_finite()
                && f.fract() == 0.0
                && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f)
            {
                Some(f as i64)
            } else {
                None
            }
        }
        _ => None,
    }
}
fn absent(value: &Cell) -> bool {
    matches!(value, Cell::Null) || integer(value) == Some(0)
}
fn text(value: &Cell) -> Option<String> {
    match value {
        Cell::Text(v) if v.len() <= 1024 && !v.contains(&0) => String::from_utf8(v.clone()).ok(),
        _ => None,
    }
}
fn project_one(
    catalog: &mut Catalog,
    source: &MigrationSource,
    policy: &Policy,
    origin: &SourceRecord,
    decision: Decision,
) -> Result<ProjectionResult> {
    catalog.project_migration_organization(
        Some(source),
        &Projection {
            origin: origin.clone(),
            import_source: policy.import_source.clone(),
            adapter_version: ADAPTER.into(),
            decision,
        },
    )
}
fn preserve(
    catalog: &mut Catalog,
    source: &MigrationSource,
    policy: &Policy,
    origin: &SourceRecord,
    construct: &str,
    compatibility: Compatibility,
    detail: &str,
) -> Result<RowResult> {
    let result = project_one(
        catalog,
        source,
        policy,
        origin,
        Decision::Retain {
            construct: construct.into(),
            compatibility,
            detail: detail.into(),
            unresolved: None,
        },
    )?;
    Ok(RowResult::Applied(Outcome::Organization(vec![result])))
}
fn mapped(catalog: &Catalog, policy: &Policy, origin: &SourceRecord) -> Result<bool> {
    Ok(catalog.db.query_row("SELECT EXISTS(SELECT 1 FROM image_import_map WHERE import_source=?1 AND capture_revision=?2 AND source_table=?3 AND source_id=?4)",params![policy.import_source,origin.source.capture_revision,origin.source.table,origin.source.identity()?],|r|r.get(0))?)
}
fn dictionary_existing(
    catalog: &Catalog,
    policy: &Policy,
    origin: &SourceRecord,
) -> Result<Option<ProjectionResult>> {
    existing_slot(catalog, policy, origin, "dictionary")
}
fn existing_slot(
    catalog: &Catalog,
    policy: &Policy,
    origin: &SourceRecord,
    slot: &str,
) -> Result<Option<ProjectionResult>> {
    let old:Option<(String,String,String)>=catalog.db.query_row("SELECT owner,adapter,result FROM migration_organization WHERE source_identity=? AND slot=?",params![origin.source.identity()?,slot],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    old.map(|(owner, adapter, value)| {
        ensure!(
            owner == policy.import_source && adapter == ADAPTER && value.len() <= 65536,
            "dictionary owner/result differs"
        );
        Ok(serde_json::from_str(&value)?)
    })
    .transpose()
}
pub(crate) fn project(
    catalog: &mut Catalog,
    source: &MigrationSource,
    policy: &Policy,
    origin: &SourceRecord,
    stage: Stage,
) -> Result<RowResult> {
    Walk::new(catalog, source, &origin.source.capture_revision)?.source_id(origin)?;
    if matches!(stage, Stage::Keywords | Stage::Collections) {
        return dictionary(catalog, source, policy, origin);
    }
    if matches!(
        stage,
        Stage::History
            | Stage::DevelopSettings
            | Stage::BeforeSettings
            | Stage::Snapshots
            | Stage::SmartCollections
    ) {
        return preserve(
            catalog,
            source,
            policy,
            origin,
            match stage {
                Stage::History => "history",
                Stage::DevelopSettings => "develop_settings",
                Stage::BeforeSettings => "before_settings",
                Stage::Snapshots => "snapshot",
                _ => "smart_collection",
            },
            if stage == Stage::SmartCollections {
                Compatibility::SmartCollectionRetained
            } else {
                Compatibility::HistoryRetained
            },
            "Original instructions are retained with source relationships; native execution differs",
        );
    }
    let walk = Walk::new(catalog, source, &origin.source.capture_revision)?;
    let Some(fields) = columns(catalog, &walk, origin)? else {
        return unsupported_row(
            catalog,
            source,
            policy,
            origin,
            &format!("uninterpreted_{stage:?}"),
        );
    };
    if stage == Stage::Stacks {
        let stack = fields
            .iter()
            .any(|(name, value)| name.to_ascii_lowercase().contains("stack") && !absent(value));
        return if stack {
            preserve(
                catalog,
                source,
                policy,
                origin,
                "stack",
                Compatibility::StackRetained,
                "Source stack fields and order are retained; native stack behavior is not equivalent",
            )
        } else {
            Ok(retained("Source image has no retained stack instruction"))
        };
    }
    if stage == Stage::ImageFields {
        if !mapped(catalog, policy, origin)? {
            return Ok(retained(
                "Organization fields have no mapped image; original row retained",
            ));
        }
        let mut decisions = Vec::new();
        for (name, value) in fields
            .iter()
            .filter(|(name, _)| matches!(name.as_str(), "rating" | "pick" | "colorLabels"))
        {
            let decision = match name.as_str() {
                "rating" => match value {
                    Cell::Null => Some(Decision::Rating { value: 0 }),
                    _ => integer(value)
                        .filter(|v| (0..=5).contains(v))
                        .map(|v| Decision::Rating { value: v as u8 }),
                },
                "pick" => match value {
                    Cell::Null => Some(Decision::Flag {
                        value: Flag::Unflagged,
                    }),
                    _ => match integer(value) {
                        Some(-1) => Some(Decision::Flag {
                            value: Flag::Reject,
                        }),
                        Some(0) => Some(Decision::Flag {
                            value: Flag::Unflagged,
                        }),
                        Some(1) => Some(Decision::Flag { value: Flag::Pick }),
                        _ => None,
                    },
                },
                "colorLabels" => match value {
                    Cell::Null => Some(Decision::Label {
                        value: String::new(),
                    }),
                    _ => text(value).map(|value| Decision::Label { value }),
                },
                _ => unreachable!(),
            };
            decisions.push(decision.unwrap_or_else(||Decision::Retain{construct:format!("uninterpreted_{name}"),compatibility:Compatibility::Unsupported,detail:format!("Source {name} value is outside the supported typed mapping; exact original bytes retained"),unresolved:None}));
        }
        let mut results = Vec::new();
        for decision in decisions {
            results.push(project_one(catalog, source, policy, origin, decision)?);
        }
        return Ok(RowResult::Applied(Outcome::Organization(results)));
    }
    let decision = match stage {
        Stage::KeywordMemberships => {
            let image = walk.link(origin, "image", "Adobe_images")?;
            let keyword = walk.link(origin, "tag", "AgLibraryKeyword")?;
            let (LinkResolution::Unique(image), LinkResolution::Unique(keyword)) = (image, keyword)
            else {
                return Ok(retained(
                    "Keyword membership has unresolved source endpoints",
                ));
            };
            if !mapped(catalog, policy, &image.target)?
                || !native_dictionary(
                    dictionary_existing(catalog, policy, &keyword.target)?,
                    false,
                )
            {
                return Ok(retained(
                    "Keyword membership endpoints lack native mappings",
                ));
            }
            Decision::KeywordMembership {
                image: *image,
                keyword: *keyword,
            }
        }
        Stage::CollectionMemberships => {
            let image = walk.link(origin, "image", "Adobe_images")?;
            let collection = walk.link(origin, "collection", "AgLibraryCollection")?;
            let (LinkResolution::Unique(image), LinkResolution::Unique(collection)) =
                (image, collection)
            else {
                return Ok(retained(
                    "Collection membership has unresolved source endpoints",
                ));
            };
            if !mapped(catalog, policy, &image.target)?
                || !native_dictionary(
                    dictionary_existing(catalog, policy, &collection.target)?,
                    true,
                )
            {
                return Ok(retained(
                    "Collection membership endpoints lack native mappings",
                ));
            }
            // No cross-type ordering proof exists for Adobe's Null/real/text
            // tokens. Neutral native order is explicit, not a numeric coercion.
            let position = 0;
            Decision::CollectionMembership {
                image: *image,
                collection: *collection,
                position,
            }
        }
        Stage::KeywordSynonyms => {
            let LinkResolution::Unique(keyword) =
                walk.link(origin, "keyword", "AgLibraryKeyword")?
            else {
                return Ok(retained(
                    "Keyword synonym has an unresolved source endpoint",
                ));
            };
            if !native_dictionary(
                dictionary_existing(catalog, policy, &keyword.target)?,
                false,
            ) {
                return Ok(retained("Keyword synonym parent lacks a native mapping"));
            }
            let Some(value) = fields.get("name").and_then(name) else {
                return preserve(
                    catalog,
                    source,
                    policy,
                    origin,
                    "synonym",
                    Compatibility::Unsupported,
                    "No supported nonempty typed synonym name; raw source row retained",
                );
            };
            let old = existing_slot(catalog, policy, origin, "synonym")?;
            if let Some(old) = old {
                // Source checkpoint replay never restores a user-deleted synonym.
                return Ok(RowResult::Applied(Outcome::Organization(vec![old])));
            }
            let Some(ProjectionResult {
                target: NativeTarget::Keyword { id },
                ..
            }) = dictionary_existing(catalog, policy, &keyword.target)?
            else {
                unreachable!()
            };
            let exists:bool=catalog.db.query_row("SELECT EXISTS(SELECT 1 FROM organization_keyword_synonyms WHERE keyword=? AND synonym=?)",params![id,value],|r|r.get(0))?;
            if exists {
                match &policy.keyword_overlap {
                    KeywordOverlap::RequireDecision=>return Ok(RowResult::NeedsDecision("Exact keyword synonym already exists; explicit hierarchy/value reuse decision required".into())),
                    KeywordOverlap::ReuseExactHierarchy{reason}=>Decision::ReuseSynonym{keyword:*keyword,value,reason:reason.clone()},
                }
            } else {
                Decision::Synonym {
                    keyword: *keyword,
                    value,
                }
            }
        }
        _ => anyhow::bail!("unsupported organization walk stage"),
    };
    let mut results = vec![project_one(catalog, source, policy, origin, decision)?];
    if stage == Stage::CollectionMemberships {
        results.push(project_one(catalog,source,policy,origin,Decision::Retain {
            construct:"collection_order".into(),compatibility:Compatibility::Unsupported,
            detail:"Native membership has neutral position 0. Exact typed positionInCollection remains addressable in this selected source row, including absent/Null, RealBits and text tokens; Adobe ordering appearance/behavior is not equivalent and no numeric-string conversion was performed.".into(),unresolved:None,
        })?);
    }
    Ok(RowResult::Applied(Outcome::Organization(results)))
}
fn dictionary(
    catalog: &mut Catalog,
    source: &MigrationSource,
    policy: &Policy,
    origin: &SourceRecord,
) -> Result<RowResult> {
    if let Some(old) = dictionary_existing(catalog, policy, origin)? {
        return if matches!(
            old.target,
            NativeTarget::Retained { .. } | NativeTarget::KeywordBoundary
        ) {
            Ok(RowResult::Applied(Outcome::Organization(vec![old])))
        } else {
            Ok(RowResult::Applied(Outcome::Organization(vec![
                old,
                behavior(catalog, source, policy, origin)?,
            ])))
        };
    }
    let mut visited = BTreeSet::new();
    let mut chain = Vec::new();
    let mut current = origin.clone();
    // At most 256 retained hierarchy edges. Each row's potentially large cells
    // are dropped before fetching its parent; only bounded decisions are kept.
    loop {
        if chain.len() == 256 {
            return preserve(
                catalog,
                source,
                policy,
                origin,
                "dictionary",
                Compatibility::Unsupported,
                "Hierarchy exceeds the 256-edge interpretation budget; exact edges retained",
            );
        }
        if !visited.insert(current.source.identity()?) {
            return preserve(
                catalog,
                source,
                policy,
                origin,
                "dictionary",
                Compatibility::Cycle,
                "Source hierarchy contains a cycle; no parent was invented",
            );
        }
        if let Some(old) = dictionary_existing(catalog, policy, &current)? {
            if matches!(old.target, NativeTarget::Retained { .. }) {
                return preserve(
                    catalog,
                    source,
                    policy,
                    origin,
                    "dictionary",
                    Compatibility::Unsupported,
                    "Ancestor has no native dictionary mapping; full hierarchy retained",
                );
            }
            break;
        }
        let walk = Walk::new(catalog, source, &origin.source.capture_revision)?;
        let Some(fields) = columns(catalog, &walk, &current)? else {
            return unsupported_row(catalog, source, policy, origin, "dictionary");
        };
        if origin.source.table == "AgLibraryCollection"
            && !matches!(
                fields.get("creationId").and_then(text).as_deref(),
                Some(
                    "com.adobe.ag.library.collection"
                        | "com.adobe.ag.library.group"
                        | "com.adobe.ag.library.smart_collection"
                )
            )
        {
            return preserve(
                catalog,
                source,
                policy,
                origin,
                "dictionary",
                Compatibility::Unsupported,
                "Unknown collection creationId; exact row and hierarchy retained without inventing native behavior",
            );
        }
        if origin.source.table == "AgLibraryKeyword"
            && organization::keyword_boundary_fields(&fields)
        {
            let retained_table = walk.schema("AgLibraryKeyword")?;
            chain.push((current, Decision::KeywordBoundary { retained_table }));
            break;
        }
        let Some(name) = fields.get("name").and_then(name) else {
            return preserve(
                catalog,
                source,
                policy,
                origin,
                "dictionary",
                Compatibility::Unsupported,
                "Dictionary has no supported nonempty typed name; full row retained",
            );
        };
        let parent = match fields.get("parent") {
            Some(value) if absent(value) => None,
            Some(_) => match walk.link(&current, "parent", &origin.source.table)? {
                LinkResolution::Unique(parent) => Some(*parent),
                other => {
                    return preserve(
                        catalog,
                        source,
                        policy,
                        origin,
                        "dictionary",
                        Compatibility::Unsupported,
                        &format!(
                            "Dictionary parent is unresolved: {}",
                            relation_description(other)
                        ),
                    );
                }
            },
            None => {
                return preserve(
                    catalog,
                    source,
                    policy,
                    origin,
                    "dictionary",
                    Compatibility::Unsupported,
                    "Dictionary schema has no parent classification; no root placement invented",
                );
            }
        };
        let next = parent.as_ref().map(|p| p.target.clone());
        let decision = if origin.source.table == "AgLibraryCollection" {
            Decision::Collection {
                name,
                parent,
                position: 0,
                decision: DictionaryDecision::Create,
            }
        } else {
            Decision::Keyword {
                name,
                keyword_kind: KeywordKind::Hierarchical,
                parent,
                decision: DictionaryDecision::Create,
            }
        };
        chain.push((current, decision));
        if let Some(next) = next {
            current = next;
        } else {
            break;
        }
    }
    let mut results = Vec::new();
    for (node, mut decision) in chain.into_iter().rev() {
        if let Decision::Keyword {
            name,
            parent,
            keyword_kind,
            decision: choice,
        } = &mut decision
        {
            let mut path = if let Some(parent) = parent {
                let (kind, path) = organization::keyword_parent_path(
                    &catalog.db,
                    &policy.import_source,
                    &parent.target,
                )?;
                ensure!(
                    kind == KeywordKind::Hierarchical,
                    "native keyword parent kind differs"
                );
                path
            } else {
                vec![]
            };
            path.push(name.clone());
            if path.len() > 64 {
                return preserve(
                    catalog,
                    source,
                    policy,
                    origin,
                    "dictionary",
                    Compatibility::Unsupported,
                    "Source keyword hierarchy exceeds native 64-component limit; exact hierarchy retained",
                );
            }
            ensure!(
                *keyword_kind == KeywordKind::Hierarchical,
                "unexpected keyword kind"
            );
            let existing: Option<i64> = catalog
                .db
                .query_row(
                    "SELECT id FROM organization_keywords WHERE kind='hierarchical' AND path=?",
                    [serde_json::to_string(&path)?],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                match &policy.keyword_overlap {
                    KeywordOverlap::RequireDecision=>return Ok(RowResult::NeedsDecision("Exact complete keyword hierarchy already exists; explicit reuse decision required".into())),
                    KeywordOverlap::ReuseExactHierarchy{reason}=>*choice=DictionaryDecision::ReuseExactHierarchy{native_id:id.to_string(),reason:reason.clone()},
                }
            }
        }
        let boundary = matches!(decision, Decision::KeywordBoundary { .. });
        results.push(project_one(catalog, source, policy, &node, decision)?);
        if !boundary {
            results.push(behavior(catalog, source, policy, &node)?);
        }
    }
    Ok(RowResult::Applied(Outcome::Organization(results)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog_images::{ImageRole, ImportImageRequest},
        catalog_migration::{lookup::Lookup, organization::Link},
        lightroom::migration_source::{SelectedCapture, tests::Fixture},
    };
    use anyhow::Context;
    const APPROVAL: &[u8] = b"synthetic selected organization walk";
    fn t(s: &str) -> Cell {
        Cell::Text(s.as_bytes().to_vec())
    }
    fn i(n: i64) -> Cell {
        Cell::Integer(n)
    }
    fn schema(table: &str) -> Vec<&'static str> {
        match table {
            "AgLibraryKeyword" => vec!["id_local", "name", "parent", "keywordType"],
            "AgLibraryKeywordSynonym" => vec!["id_local", "name", "keyword"],
            "AgLibraryCollection" => vec!["id_local", "name", "parent", "creationId", "systemOnly"],
            "AgLibraryCollectionImage" => {
                vec!["id_local", "image", "collection", "positionInCollection"]
            }
            "AgLibraryKeywordImage" => vec!["id_local", "image", "tag"],
            "Adobe_images" => vec!["id_local", "rating", "pick", "colorLabels"],
            _ => vec!["id_local"],
        }
    }
    struct Bed {
        _fixture: Fixture,
        _temp: tempfile::TempDir,
        source: MigrationSource,
        catalog: Catalog,
        revisions: Vec<String>,
        policy: Policy,
    }
    impl Bed {
        fn new(large: bool) -> Result<Self> {
            Self::build(large, false)
        }
        fn build(large: bool, boundary: bool) -> Result<Self> {
            let mut fixture = Fixture::new();
            let second = fixture.seal.excluded_revisions.pop().unwrap();
            let db = rusqlite::Connection::open(&fixture.path)?;
            let raw: String = db.query_row(
                "SELECT manifest FROM captures WHERE revision=?",
                [&second],
                |r| r.get(0),
            )?;
            drop(db);
            fixture.seal.selected.push(SelectedCapture {
                revision: second.clone(),
                family: "second family".into(),
                family_evidence_digest: "a".repeat(64),
                manifest_blake3: blake3::hash(raw.as_bytes()).to_hex().to_string(),
                evidence_revision: 9,
            });
            fixture.seal.approval.document_blake3 = blake3::hash(APPROVAL).to_hex().to_string();
            let revisions = fixture
                .seal
                .selected
                .iter()
                .map(|s| s.revision.clone())
                .collect::<Vec<_>>();
            fixture.edit(|db| {
                db.execute("INSERT INTO family_choices VALUES('second family',?,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','fixture')",[&second]).unwrap();
                for (rindex,revision) in revisions.iter().enumerate() {
                    let mut rows:Vec<(i64,&str,Vec<Cell>)>=vec![
                        (1,"AgLibraryKeyword",vec![i(1),if boundary {Cell::Null} else {t("Root")},Cell::Null,Cell::Null]),
                        (2,"AgLibraryKeyword",vec![i(2),t("Child"),i(1),t("person")]),
                    ];
                    if boundary {
                        rows.extend([
                            (14,"AgLibraryKeyword",vec![i(14),t("Grandchild"),i(2),Cell::Null]),
                            (15,"AgLibraryKeyword",vec![i(15),t(""),Cell::Null,Cell::Null]),
                            (16,"AgLibraryKeyword",vec![i(16),Cell::Null,i(2),Cell::Null]),
                        ]);
                        if rindex==0 {
                            rows.extend([
                                (73,"AgLibraryKeywordImage",vec![i(73),i(40),i(1)]),
                                (23,"AgLibraryKeywordSynonym",vec![i(23),t("root alias"),i(1)]),
                            ]);
                        }
                    }
                    if rindex==0 {
                        rows.extend([
                            (3,"AgLibraryKeyword",vec![i(3),t("Other"),Cell::Null,Cell::Null]),
                            (4,"AgLibraryKeyword",vec![i(4),t("Child"),i(3),Cell::Null]),
                            (5,"AgLibraryKeyword",vec![i(5),t("CycleA"),i(6),Cell::Null]),
                            (6,"AgLibraryKeyword",vec![i(6),t("CycleB"),i(5),Cell::Null]),
                            (7,"AgLibraryKeyword",vec![i(7),Cell::Blob(vec![255]),Cell::Null,Cell::Null]),
                            (8,"AgLibraryKeyword",vec![i(8),t("Bad ancestor"),i(7),Cell::Null]),
                            (10,"AgLibraryKeyword",vec![i(10),t("Missing"),i(999),Cell::Null]),
                            (11,"AgLibraryKeyword",vec![i(11),t("Ambiguous"),i(12),Cell::Null]),
                            (12,"AgLibraryKeyword",vec![i(12),t("First"),Cell::Null,Cell::Null]),
                            (13,"AgLibraryKeyword",vec![i(13),t("Second"),Cell::Null,Cell::Null]),
                            (20,"AgLibraryKeywordSynonym",vec![i(20),t("alias"),i(2)]),
                            (21,"AgLibraryKeywordSynonym",vec![i(21),t("alias"),i(2)]),
                            (22,"AgLibraryKeywordSynonym",vec![i(22),t("cycle alias"),i(5)]),
                            (30,"AgLibraryCollection",vec![i(30),t("Group"),Cell::Null,t("com.adobe.ag.library.group"),i(0)]),
                            (31,"AgLibraryCollection",vec![i(31),t("Same"),i(30),t("com.adobe.ag.library.collection"),i(1)]),
                            (32,"AgLibraryCollection",vec![i(32),t("Same"),i(30),t("com.adobe.ag.library.smart_collection"),i(0)]),
                            (33,"AgLibraryCollection",vec![i(33),t("Unknown"),Cell::Null,t("unknown.kind"),i(0)]),
                            (34,"AgLibraryCollection",vec![i(34),t("Cycle"),i(35),t("com.adobe.ag.library.collection"),i(0)]),
                            (35,"AgLibraryCollection",vec![i(35),t("Cycle"),i(34),t("com.adobe.ag.library.collection"),i(0)]),
                            (60,"AgLibraryCollectionImage",vec![i(60),i(40),i(34),Cell::Null]),
                            (70,"AgLibraryKeywordImage",vec![i(70),i(40),i(5)]),
                            (71,"AgLibraryKeywordImage",vec![i(71),i(40),i(2)]),
                            (72,"AgLibraryKeywordImage",vec![i(72),i(41),i(2)]),
                        ]);
                        for (n,position) in [Cell::Null,Cell::RealBits(2.0f64.to_bits()),t("---0"),t("---1"),t("V"),t("V--0"),t("V--F")].into_iter().enumerate() {
                            let n=n as i64;
                            rows.push((40+n,"Adobe_images",vec![i(40+n),i(n%6),i(if n==0{1}else{-1}),t("Blue")]));
                            rows.push((50+n,"AgLibraryCollectionImage",vec![i(50+n),i(40+n),i(31),position]));
                        }
                        if large {rows.push((9,"AgLibraryKeyword",vec![i(9),Cell::Text(vec![b'x';LIMIT/2+100]),Cell::Null,Cell::Null]));}
                    }
                    for (id,table,cells) in &rows {
                        db.execute("INSERT OR IGNORE INTO tables(revision,name,columns_json,key_json,schema_json,category,expected,retained,state) VALUES(?1,?2,?3,'[]','{}','fixture',0,0,'complete')",params![revision,table,serde_json::to_string(&schema(table)).unwrap()]).unwrap();
                        db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?1,?2,?3,?4,?5)",params![revision,format!("org-{id}"),table,serde_json::to_string(&vec![i(*id)]).unwrap(),serde_json::to_string(cells).unwrap()]).unwrap();
                        let local=if *id==13{12}else{*id};
                        db.execute("INSERT INTO entities VALUES(?1,?2,?3,?4,NULL,'{}')",params![revision,format!("org-{id}"),table,serde_json::to_string(&i(local)).unwrap()]).unwrap();
                        for (field,value) in schema(table).into_iter().zip(cells) {
                            let target=match (*table,field) {
                                ("AgLibraryKeyword","parent")|( "AgLibraryKeywordSynonym","keyword")|( "AgLibraryKeywordImage","tag")=>Some("AgLibraryKeyword"),
                                ("AgLibraryCollection","parent")|( "AgLibraryCollectionImage","collection")=>Some("AgLibraryCollection"),
                                (_,"image")=>Some("Adobe_images"),_=>None,
                            };
                            if let (Some(target),Cell::Integer(key))=(target,value) && *key!=0 {
                                db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,?2,?3,?4,?5)",params![revision,format!("org-{id}"),field,target,serde_json::to_string(&i(*key)).unwrap()]).unwrap();
                            }
                        }
                    }
                }
                db.execute("UPDATE tables SET expected=(SELECT count(*) FROM rows r WHERE r.revision=tables.revision AND r.table_name=tables.name),retained=(SELECT count(*) FROM rows r WHERE r.revision=tables.revision AND r.table_name=tables.name)",[]).unwrap();
            });
            let source = fixture.open();
            let temp = tempfile::tempdir()?;
            let mut catalog = Catalog::open(temp.path().join("catalog"))?;
            catalog.begin_migration_retention(&source, APPROVAL)?;
            for _ in 0..1000 {
                if catalog.step_migration_retention(&source)?.complete {
                    break;
                }
            }
            ensure!(
                catalog
                    .migration_retention_progress(source.binding_blake3())?
                    .complete,
                "custody did not finish"
            );
            Ok(Self {
                _fixture: fixture,
                _temp: temp,
                source,
                catalog,
                revisions,
                policy: Policy {
                    import_source: "lightroom".into(),
                    artifacts: vec![],
                    supplements: vec![],
                    overlap: super::super::importer::OverlapPolicy::RequireDecision,
                    keyword_overlap: KeywordOverlap::ReuseExactHierarchy {
                        reason: "explicit synthetic full-hierarchy reuse".into(),
                    },
                },
            })
        }
        fn row(&self, family: usize, id: i64) -> Result<SourceRecord> {
            let w = Walk::new(&self.catalog, &self.source, &self.revisions[family])?;
            w.source_record(
                w.singleton(&Lookup::RowsBySource(format!("org-{id}")))?
                    .context("missing fixture row")?,
            )
        }
        fn project(&mut self, family: usize, id: i64, stage: Stage) -> Result<RowResult> {
            let row = self.row(family, id)?;
            project(&mut self.catalog, &self.source, &self.policy, &row, stage)
        }
        fn target(&self, family: usize, id: i64, slot: &str) -> Result<NativeTarget> {
            Ok(self
                .catalog
                .migration_organization_projection(&self.row(family, id)?.source, slot)?
                .context("missing projection")?
                .target)
        }
        fn images(&mut self) -> Result<()> {
            self.catalog.db.execute("INSERT INTO assets(id,location,path_display,state) VALUES('fixture-file',X'2F6D697373696E67','/missing','pending')",[])?;
            let mut master = None;
            for id in 40..47 {
                let row = self.row(0, id)?;
                let value = self.catalog.register_import_image(&ImportImageRequest {
                    import_source: "lightroom".into(),
                    capture_revision: row.source.capture_revision.clone(),
                    source_table: row.source.table.clone(),
                    source_id: row.source.identity()?,
                    input_digest: format!("fixture-{id}"),
                    adapter_version: "fixture".into(),
                    asset_id: "fixture-file".into(),
                    claim_reserved_master: false,
                    role: if id == 40 {
                        ImageRole::Master
                    } else {
                        ImageRole::Virtual
                    },
                    master: master.clone(),
                    label: format!("Image {id}"),
                })?;
                if id == 40 {
                    master = Some(value.key);
                }
            }
            Ok(())
        }
    }
    #[test]
    fn walk_cycles_invalid_ancestors_and_large_rows_are_retained() -> Result<()> {
        let mut b = Bed::new(true)?;
        for id in [5, 8, 9, 10, 11] {
            assert!(matches!(
                b.project(0, id, Stage::Keywords)?,
                RowResult::Applied(_)
            ));
            assert!(matches!(
                b.target(0, id, "dictionary")?,
                NativeTarget::Retained { .. }
            ));
        }
        assert!(matches!(
            b.target(0, 5, "dictionary")?,
            NativeTarget::Retained {
                compatibility: Compatibility::Cycle
            }
        ));
        assert!(matches!(
            b.project(0, 2, Stage::Keywords)?,
            RowResult::Applied(_)
        ));
        Ok(())
    }
    #[test]
    fn walk_full_hierarchy_reuse_is_explicit_and_resume_preserves_user_changes() -> Result<()> {
        let mut b = Bed::new(false)?;
        b.project(0, 2, Stage::Keywords)?;
        b.project(0, 4, Stage::Keywords)?;
        assert_ne!(b.target(0, 2, "dictionary")?, b.target(0, 4, "dictionary")?);
        b.policy.keyword_overlap = KeywordOverlap::RequireDecision;
        assert!(matches!(
            b.project(1, 2, Stage::Keywords)?,
            RowResult::NeedsDecision(_)
        ));
        assert!(dictionary_existing(&b.catalog, &b.policy, &b.row(1, 2)?)?.is_none());
        b.policy.keyword_overlap = KeywordOverlap::ReuseExactHierarchy {
            reason: "explicit second family reuse".into(),
        };
        b.project(1, 2, Stage::Keywords)?;
        assert_eq!(b.target(0, 2, "dictionary")?, b.target(1, 2, "dictionary")?);
        let NativeTarget::Keyword { id } = b.target(0, 2, "dictionary")? else {
            unreachable!()
        };
        b.catalog.db.execute(
            "UPDATE organization_keywords SET name='User name' WHERE id=?",
            [id],
        )?;
        b.project(1, 2, Stage::Keywords)?;
        let name: String = b.catalog.db.query_row(
            "SELECT name FROM organization_keywords WHERE id=?",
            [id],
            |r| r.get(0),
        )?;
        assert_eq!(name, "User name");
        Ok(())
    }
    #[test]
    fn walk_duplicate_synonyms_have_distinct_custody_without_reinsert() -> Result<()> {
        let mut b = Bed::new(false)?;
        b.project(0, 2, Stage::Keywords)?;
        b.project(0, 20, Stage::KeywordSynonyms)?;
        b.policy.keyword_overlap = KeywordOverlap::RequireDecision;
        assert!(matches!(
            b.project(0, 21, Stage::KeywordSynonyms)?,
            RowResult::NeedsDecision(_)
        ));
        b.policy.keyword_overlap = KeywordOverlap::ReuseExactHierarchy {
            reason: "explicit synonym reuse".into(),
        };
        b.project(0, 21, Stage::KeywordSynonyms)?;
        assert_eq!(b.target(0, 20, "synonym")?, b.target(0, 21, "synonym")?);
        let count: i64 = b.catalog.db.query_row(
            "SELECT count(*) FROM organization_keyword_synonyms WHERE synonym='alias'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 1);
        b.catalog.db.execute(
            "DELETE FROM organization_keyword_synonyms WHERE synonym='alias'",
            [],
        )?;
        b.project(0, 20, Stage::KeywordSynonyms)?;
        b.project(0, 21, Stage::KeywordSynonyms)?;
        let count: i64 = b.catalog.db.query_row(
            "SELECT count(*) FROM organization_keyword_synonyms WHERE synonym='alias'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 0);
        Ok(())
    }
    #[test]
    fn walk_retained_endpoints_never_reach_native_membership_or_synonym() -> Result<()> {
        let mut b = Bed::new(false)?;
        b.images()?;
        b.project(0, 5, Stage::Keywords)?;
        b.project(0, 34, Stage::Collections)?;
        for (id, stage) in [
            (70, Stage::KeywordMemberships),
            (22, Stage::KeywordSynonyms),
            (60, Stage::CollectionMemberships),
        ] {
            assert!(matches!(
                b.project(0, id, stage)?,
                RowResult::Applied(Outcome::Retained { .. })
            ));
        }
        Ok(())
    }
    #[test]
    fn walk_every_typed_order_token_keeps_native_membership_and_explicit_gap() -> Result<()> {
        let mut b = Bed::new(false)?;
        b.images()?;
        b.project(0, 31, Stage::Collections)?;
        b.project(0, 32, Stage::Collections)?;
        b.project(0, 33, Stage::Collections)?;
        assert_ne!(
            b.target(0, 31, "dictionary")?,
            b.target(0, 32, "dictionary")?
        );
        assert!(matches!(
            b.target(0, 33, "dictionary")?,
            NativeTarget::Retained { .. }
        ));
        for id in 50..57 {
            b.project(0, id, Stage::CollectionMemberships)?;
            assert!(matches!(
                b.target(0, id, "collection_membership")?,
                NativeTarget::Image { .. }
            ));
            assert!(matches!(
                b.target(0, id, "collection_order")?,
                NativeTarget::Retained { .. }
            ));
        }
        let count: i64 = b.catalog.db.query_row(
            "SELECT count(*) FROM organization_collection_members",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 7);
        b.catalog
            .db
            .execute("UPDATE organization_collection_order SET position=42", [])?;
        for id in 50..57 {
            b.project(0, id, Stage::CollectionMemberships)?;
        }
        let count: i64 = b.catalog.db.query_row(
            "SELECT count(*) FROM organization_collection_order WHERE position=42",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 7);
        Ok(())
    }
    #[test]
    fn walk_virtual_flags_and_keyword_memberships_are_independent_and_replay_safe() -> Result<()> {
        let mut b = Bed::new(false)?;
        b.images()?;
        b.project(0, 2, Stage::Keywords)?;
        for (id, stage) in [
            (40, Stage::ImageFields),
            (41, Stage::ImageFields),
            (71, Stage::KeywordMemberships),
            (72, Stage::KeywordMemberships),
        ] {
            b.project(0, id, stage)?;
        }
        let flags: Vec<String> = b
            .catalog
            .db
            .prepare("SELECT flag FROM organization_assets ORDER BY sequence")?
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        assert!(flags.contains(&"pick".into()) && flags.contains(&"reject".into()));
        let before: i64 =
            b.catalog
                .db
                .query_row("SELECT sum(revision) FROM metadata_assets", [], |r| {
                    r.get(0)
                })?;
        for (id, stage) in [
            (40, Stage::ImageFields),
            (41, Stage::ImageFields),
            (71, Stage::KeywordMemberships),
            (72, Stage::KeywordMemberships),
        ] {
            b.project(0, id, stage)?;
        }
        let after: i64 =
            b.catalog
                .db
                .query_row("SELECT sum(revision) FROM metadata_assets", [], |r| {
                    r.get(0)
                })?;
        assert_eq!(before, after);
        Ok(())
    }
    #[test]
    fn walk_mapping_failure_rolls_back_new_dictionary_and_reuse_checkpoint() -> Result<()> {
        let mut b = Bed::new(false)?;
        let child = b.row(0, 2)?.source.identity()?;
        b.catalog.db.execute_batch(&format!("CREATE TRIGGER reject_org BEFORE INSERT ON migration_organization WHEN NEW.source_identity='{child}' BEGIN SELECT RAISE(ABORT,'fixture interruption'); END;"))?;
        assert!(b.project(0, 2, Stage::Keywords).is_err());
        let count: i64 =
            b.catalog
                .db
                .query_row("SELECT count(*) FROM organization_keywords", [], |r| {
                    r.get(0)
                })?;
        assert_eq!(count, 1);
        b.catalog.db.execute_batch("DROP TRIGGER reject_org")?;
        b.project(0, 2, Stage::Keywords)?;
        let count: i64 =
            b.catalog
                .db
                .query_row("SELECT count(*) FROM organization_keywords", [], |r| {
                    r.get(0)
                })?;
        assert_eq!(count, 2);
        b.project(0, 20, Stage::KeywordSynonyms)?;
        let synonym = b.row(0, 21)?.source.identity()?;
        b.catalog.db.execute_batch(&format!("CREATE TRIGGER reject_org BEFORE INSERT ON migration_organization WHEN NEW.source_identity='{synonym}' BEGIN SELECT RAISE(ABORT,'fixture interruption'); END;"))?;
        assert!(b.project(0, 21, Stage::KeywordSynonyms).is_err());
        assert!(existing_slot(&b.catalog, &b.policy, &b.row(0, 21)?, "synonym")?.is_none());
        b.catalog.db.execute_batch("DROP TRIGGER reject_org")?;
        b.project(0, 21, Stage::KeywordSynonyms)?;
        let count: i64 = b.catalog.db.query_row(
            "SELECT count(*) FROM organization_keyword_synonyms",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 1);
        Ok(())
    }
    #[test]
    fn walk_invalid_source_proof_is_an_error_not_a_retained_classification() -> Result<()> {
        let mut b = Bed::new(false)?;
        let mut row = b.row(0, 2)?;
        row.source.key = vec![i(99999)];
        assert!(project(&mut b.catalog, &b.source, &b.policy, &row, Stage::Keywords).is_err());
        assert_eq!(
            b.catalog
                .db
                .query_row("SELECT count(*) FROM migration_organization", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        Ok(())
    }
    #[test]
    fn keyword_boundary_preserves_named_hierarchy_and_source_link() -> Result<()> {
        // The sentinel is deliberately id 1, not the observed catalog's id 20.
        let mut b = Bed::build(false, true)?;
        b.project(0, 14, Stage::Keywords)?;
        assert_eq!(b.target(0, 1, "dictionary")?, NativeTarget::KeywordBoundary);
        for (local, expected) in [(2, vec!["Child"]), (14, vec!["Child", "Grandchild"])] {
            let NativeTarget::Keyword { id } = b.target(0, local, "dictionary")? else {
                anyhow::bail!("named keyword missing");
            };
            let raw: String = b.catalog.db.query_row(
                "SELECT path FROM organization_keywords WHERE id=?",
                [id],
                |r| r.get(0),
            )?;
            assert_eq!(serde_json::from_str::<Vec<String>>(&raw)?, expected);
        }
        let child = b.row(0, 2)?;
        let proof: String = b.catalog.db.query_row(
            "SELECT proof FROM migration_organization WHERE source_identity=? AND slot='dictionary'",
            [child.source.identity()?], |r| r.get(0),
        )?;
        let proof: serde_json::Value = serde_json::from_str(&proof)?;
        let parent: Link = serde_json::from_value(proof["request"]["decision"]["parent"].clone())?;
        assert_eq!(parent.target.source, b.row(0, 1)?.source);
        assert_eq!(parent.field, "parent");
        let count: i64 =
            b.catalog
                .db
                .query_row("SELECT count(*) FROM organization_keywords", [], |r| {
                    r.get(0)
                })?;
        assert_eq!(count, 2);
        // Exact hierarchy reuse remains governed by the existing policy.
        b.policy.keyword_overlap = KeywordOverlap::RequireDecision;
        assert!(matches!(
            b.project(1, 14, Stage::Keywords)?,
            RowResult::NeedsDecision(_)
        ));
        b.policy.keyword_overlap = KeywordOverlap::ReuseExactHierarchy {
            reason: "same full named hierarchy".into(),
        };
        b.project(1, 14, Stage::Keywords)?;
        assert_eq!(
            b.target(0, 14, "dictionary")?,
            b.target(1, 14, "dictionary")?
        );
        // Offline root replay proves the same raw row/schema without a source lease.
        let root = b.row(0, 1)?;
        let table =
            Walk::new(&b.catalog, &b.source, &b.revisions[0])?.schema("AgLibraryKeyword")?;
        let replay = b.catalog.project_migration_organization(
            None,
            &Projection {
                origin: root,
                import_source: b.policy.import_source.clone(),
                adapter_version: ADAPTER.into(),
                decision: Decision::KeywordBoundary {
                    retained_table: table,
                },
            },
        )?;
        assert_eq!(replay.target, NativeTarget::KeywordBoundary);
        Ok(())
    }

    #[test]
    fn keyword_boundary_is_not_a_membership_or_synonym_endpoint() -> Result<()> {
        let mut b = Bed::build(false, true)?;
        b.images()?;
        b.project(0, 1, Stage::Keywords)?;
        for (id, stage) in [
            (73, Stage::KeywordMemberships),
            (23, Stage::KeywordSynonyms),
        ] {
            assert!(matches!(
                b.project(0, id, stage)?,
                RowResult::Applied(Outcome::Retained { .. })
            ));
        }
        // Even a direct caller cannot turn the boundary into a metadata keyword.
        let origin = b.row(0, 73)?;
        let walk = Walk::new(&b.catalog, &b.source, &b.revisions[0])?;
        let LinkResolution::Unique(image) = walk.link(&origin, "image", "Adobe_images")? else {
            anyhow::bail!("fixture image");
        };
        let LinkResolution::Unique(keyword) = walk.link(&origin, "tag", "AgLibraryKeyword")? else {
            anyhow::bail!("fixture keyword");
        };
        assert!(
            project_one(
                &mut b.catalog,
                &b.source,
                &b.policy,
                &origin,
                Decision::KeywordMembership {
                    image: *image,
                    keyword: *keyword
                }
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn keyword_boundary_rejects_invalid_and_cross_capture_proofs() -> Result<()> {
        let mut b = Bed::build(false, true)?;
        for id in [5, 8, 10, 11, 15, 16] {
            b.project(0, id, Stage::Keywords)?;
            assert!(matches!(
                b.target(0, id, "dictionary")?,
                NativeTarget::Retained { .. }
            ));
        }
        let table =
            Walk::new(&b.catalog, &b.source, &b.revisions[0])?.schema("AgLibraryKeyword")?;
        let foreign_table =
            Walk::new(&b.catalog, &b.source, &b.revisions[1])?.schema("AgLibraryKeyword")?;
        let root = b.row(0, 1)?;
        assert!(
            project_one(
                &mut b.catalog,
                &b.source,
                &b.policy,
                &root,
                Decision::KeywordBoundary {
                    retained_table: foreign_table
                }
            )
            .is_err()
        );
        let named = b.row(0, 3)?;
        assert!(
            project_one(
                &mut b.catalog,
                &b.source,
                &b.policy,
                &named,
                Decision::KeywordBoundary {
                    retained_table: table
                }
            )
            .is_err()
        );
        let child = b.row(0, 2)?;
        let walk = Walk::new(&b.catalog, &b.source, &b.revisions[0])?;
        let LinkResolution::Unique(mut link) = walk.link(&child, "parent", "AgLibraryKeyword")?
        else {
            anyhow::bail!("fixture parent");
        };
        link.target = b.row(1, 1)?;
        assert!(
            project_one(
                &mut b.catalog,
                &b.source,
                &b.policy,
                &child,
                Decision::Keyword {
                    name: "Child".into(),
                    keyword_kind: KeywordKind::Hierarchical,
                    parent: Some(*link),
                    decision: DictionaryDecision::Create
                }
            )
            .is_err()
        );
        // Missing columns, empty strings and non-root nulls are never boundaries.
        for fields in [
            BTreeMap::new(),
            BTreeMap::from([("name".into(), Cell::Null)]),
            BTreeMap::from([("name".into(), t("")), ("parent".into(), Cell::Null)]),
            BTreeMap::from([("name".into(), Cell::Null), ("parent".into(), i(1))]),
        ] {
            assert!(!organization::keyword_boundary_fields(&fields));
        }
        Ok(())
    }
}
