//! Catalog-owned preview storage settings and explicit incremental relocation.
use super::{BridgeError, ErrorCode, U64, error, native};
use crate::{
    Catalog, catalog_export_alias,
    catalog_writer::Priority,
    preview::{PreviewService, Tier},
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const ROOT_LIMIT: usize = 1024;
const ROOT_BYTES: usize = 2 * 1024 * 1024;
const DIRECTORY_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "command",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    Status,
    SetBudgets {
        thumbnail_bytes: U64,
        large_bytes: U64,
    },
    BeginOriginalRootReview {
        roots: Vec<NativePath>,
    },
    StepOriginalRootReview {
        review: String,
        directories: u16,
    },
    BeginRelocation {
        tier: Tier,
        destination: NativePath,
    },
    StepRelocation {
        tier: Tier,
        objects: u16,
        bytes: U64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub thumbnail_root: NativePath,
    pub large_root: NativePath,
    pub thumbnail_bytes: U64,
    pub large_bytes: U64,
    pub relocation_pending: bool,
    pub relocation_tier: Option<Tier>,
    pub relocation: Option<Relocation>,
    pub original_roots: OriginalRoots,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relocation {
    pub source: NativePath,
    pub destination: NativePath,
    pub phase: String,
    pub objects: Option<U64>,
    pub bytes: Option<U64>,
    pub total_objects: Option<U64>,
    pub total_bytes: Option<U64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OriginalRoots {
    pub state: String,
    pub roots: Vec<NativePath>,
    pub review: Option<String>,
    pub checked_directories: U64,
    pub uncovered: Option<NativePath>,
    pub message: Option<String>,
}

#[derive(Default)]
pub(super) struct State {
    review: Option<Review>,
}

struct Review {
    id: String,
    roots: Vec<PathBuf>,
    storage_epoch: u64,
    after: i64,
    checked: u64,
    projecting: bool,
    issue: Option<String>,
    uncovered: Option<NativePath>,
}

struct CoverageBatch {
    scanned_through: i64,
    checked: u64,
    pending: bool,
    unbound: u64,
    complete: bool,
    uncovered: Option<NativePath>,
}

fn storage_epoch(catalog: &Catalog) -> Result<u64> {
    u64::try_from(catalog.db.query_row(
        "SELECT revision FROM storage_epoch WHERE id=1",
        [],
        |row| row.get::<_, i64>(0),
    )?)
    .context("invalid catalog storage epoch")
}

fn normalize_roots(service: &PreviewService, roots: Vec<NativePath>) -> Result<Vec<PathBuf>> {
    ensure!(!roots.is_empty(), "choose at least one original-photo root");
    ensure!(
        roots.len() <= ROOT_LIMIT,
        "original root count exceeds bound"
    );
    let mut result = Vec::with_capacity(roots.len());
    let mut bytes = 0usize;
    for native in roots {
        let encoded = serde_json::to_vec(&native)?;
        ensure!(
            encoded.len() <= 256 * 1024,
            "original root exceeds byte bound"
        );
        bytes = bytes
            .checked_add(encoded.len())
            .filter(|value| *value <= ROOT_BYTES)
            .context("original root bytes exceed bound")?;
        let path = native.to_path()?;
        ensure!(path.is_absolute(), "original root must be absolute");
        let path = std::fs::canonicalize(path).context("resolve original-photo root")?;
        ensure!(path.is_dir(), "original-photo root must be a directory");
        service.ensure_original_separate(&path)?;
        if !result.contains(&path) {
            result.push(path);
        }
    }
    ensure!(
        !result.is_empty(),
        "choose at least one original-photo root"
    );
    Ok(result)
}

fn coverage_batch(
    catalog: &mut Catalog,
    roots: &[PathBuf],
    expected_epoch: u64,
    after: i64,
    limit: usize,
) -> Result<CoverageBatch> {
    ensure!(
        (1..=256).contains(&limit),
        "root review batch must contain 1–256 directories"
    );
    catalog.require_jobs_released()?;
    let _write = catalog.writers.enter(Priority::Foreground)?;
    let tx = catalog
        .db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let epoch = u64::try_from(tx.query_row(
        "SELECT revision FROM storage_epoch WHERE id=1",
        [],
        |row| row.get::<_, i64>(0),
    )?)?;
    ensure!(
        epoch == expected_epoch,
        "catalog original locations changed; restart the root review"
    );
    let alias = catalog_export_alias::reconcile_paths(&tx, 512)?;
    let mut batch = CoverageBatch {
        scanned_through: after,
        checked: 0,
        pending: alias.pending,
        unbound: alias.unbound,
        complete: false,
        uncovered: None,
    };
    if !alias.pending && alias.unbound == 0 {
        let mut statement = tx.prepare(
            "SELECT id,CASE WHEN length(CAST(native_path AS BLOB))<=262144 THEN length(CAST(native_path AS BLOB)) ELSE NULL END FROM export_alias_directories INDEXED BY export_alias_active_directories WHERE members>0 AND id>?1 ORDER BY id LIMIT ?2",
        )?;
        let mut rows = statement.query(params![after, i64::try_from(limit + 1)?])?;
        let mut candidates = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let length = usize::try_from(row.get::<_, i64>(1)?)?;
            candidates.push((id, length));
        }
        drop(rows);
        drop(statement);
        let mut bytes = 0usize;
        let mut consumed = 0usize;
        for (id, length) in candidates.iter().take(limit) {
            if bytes
                .checked_add(*length)
                .is_none_or(|next| next > DIRECTORY_BYTES)
            {
                break;
            }
            let encoded: String = tx.query_row(
                "SELECT CASE WHEN length(CAST(native_path AS BLOB))=?2 THEN native_path ELSE NULL END FROM export_alias_directories WHERE id=?1 AND members>0",
                params![id, i64::try_from(*length)?],
                |row| row.get(0),
            )?;
            ensure!(
                encoded.len() == *length,
                "original directory encoding changed while reviewing"
            );
            let native = serde_json::from_str::<NativePath>(&encoded)?;
            let directory = native.to_path().context(
                "catalog original paths use another platform encoding; relink them before reviewing preview storage",
            )?;
            bytes += *length;
            consumed += 1;
            batch.scanned_through = *id;
            batch.checked = batch
                .checked
                .checked_add(1)
                .context("root review count overflow")?;
            // Stored native paths may contain an existing directory alias (for
            // example /var on macOS); compare the same prospective canonical
            // namespace used for reviewed roots, without opening regular files.
            let resolved_directory = crate::prospective_directory(&directory)?;
            if !roots
                .iter()
                .any(|root| resolved_directory.starts_with(root))
            {
                batch.uncovered = Some(native);
                break;
            }
        }
        ensure!(
            consumed != 0 || candidates.is_empty(),
            "original directory exceeds review byte bound"
        );
        let has_more = consumed < candidates.len();
        batch.complete = batch.uncovered.is_none() && !has_more;
    }
    let final_epoch = u64::try_from(tx.query_row(
        "SELECT revision FROM storage_epoch WHERE id=1",
        [],
        |row| row.get::<_, i64>(0),
    )?)?;
    ensure!(
        final_epoch == expected_epoch,
        "catalog original locations changed; restart the root review"
    );
    tx.commit()?;
    Ok(batch)
}

#[cfg(test)]
pub(super) fn execute(
    catalog: &mut Catalog,
    service: &mut PreviewService,
    state: &mut State,
    request: Request,
) -> Result<Status, BridgeError> {
    execute_with_root_admission(catalog, service, state, request, &mut |_| Ok(()))
}

pub(super) fn execute_with_root_admission(
    catalog: &mut Catalog,
    service: &mut PreviewService,
    state: &mut State,
    request: Request,
    admit_roots: &mut dyn FnMut(&[PathBuf]) -> Result<(), BridgeError>,
) -> Result<Status, BridgeError> {
    match request {
        Request::Status => {}
        Request::SetBudgets {
            thumbnail_bytes,
            large_bytes,
        } => {
            service
                .set_cache_budgets(thumbnail_bytes.0, large_bytes.0)
                .map_err(native)?;
        }
        Request::BeginOriginalRootReview { roots } => {
            let roots = normalize_roots(service, roots).map_err(native)?;
            state.review = Some(Review {
                id: uuid::Uuid::new_v4().to_string(),
                roots,
                storage_epoch: storage_epoch(catalog).map_err(native)?,
                after: 0,
                checked: 0,
                projecting: true,
                issue: None,
                uncovered: None,
            });
        }
        Request::StepOriginalRootReview {
            review,
            directories,
        } => {
            if !(1..=256).contains(&directories) {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "root review batch must contain 1–256 directories",
                ));
            }
            let active = state.review.as_mut().ok_or_else(|| {
                error(
                    ErrorCode::StaleSession,
                    "original-root review is not active",
                )
            })?;
            if active.id != review {
                return Err(error(
                    ErrorCode::StaleSession,
                    "original-root review belongs to another session",
                ));
            }
            let mut completed = None;
            if storage_epoch(catalog).map_err(native)? != active.storage_epoch {
                state.review = None;
            } else if active.issue.is_none() {
                let batch = coverage_batch(
                    catalog,
                    &active.roots,
                    active.storage_epoch,
                    active.after,
                    usize::from(directories),
                )
                .map_err(native)?;
                active.after = batch.scanned_through;
                active.checked = active
                    .checked
                    .checked_add(batch.checked)
                    .ok_or_else(|| error(ErrorCode::ResourceLimit, "root review count overflow"))?;
                active.projecting = batch.pending;
                if batch.unbound != 0 {
                    active.issue = Some(format!(
                        "{count} catalog photos do not have reviewed native locations. Relink or map them, then restart this review.",
                        count = batch.unbound
                    ));
                } else if let Some(uncovered) = batch.uncovered {
                    active.uncovered = Some(uncovered);
                    active.issue = Some(
                        "A catalog original lies outside the selected roots. Add its enclosing original-photo root and restart this review."
                            .into(),
                    );
                } else if batch.complete {
                    completed = Some((active.roots.clone(), active.storage_epoch));
                }
            }
            if let Some((roots, expected_epoch)) = completed {
                // F independently validates and retains these user-reviewed roots
                // before the cache manifest advertises the review as ready. A
                // changed catalog cannot turn an earlier coverage result into new
                // path authority while admission is in flight.
                admit_roots(&roots)?;
                if storage_epoch(catalog).map_err(native)? != expected_epoch {
                    state.review = None;
                    return Err(error(
                        ErrorCode::StaleSession,
                        "catalog original locations changed; restart the root review",
                    ));
                }
                service
                    .replace_original_root_review(&roots, expected_epoch)
                    .map_err(native)?;
                state.review = None;
            }
        }
        Request::BeginRelocation { tier, destination } => {
            if !service.is_drained() {
                return Err(error(
                    ErrorCode::Busy,
                    "finish or cancel preview requests before moving previews",
                ));
            }
            let destination = destination.to_path().map_err(|e| native(e.into()))?;
            if !destination.is_absolute() {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "preview destination must be absolute",
                ));
            }
            let epoch = storage_epoch(catalog).map_err(native)?;
            let review = service.original_root_review().map_err(native)?;
            if review.roots.is_empty() || review.storage_epoch != Some(epoch) {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "review every original-photo root for the current catalog before moving previews",
                ));
            }
            service
                .begin_relocation(tier, &destination, &review.roots)
                .map_err(native)?;
        }
        Request::StepRelocation {
            tier,
            objects,
            bytes,
        } => {
            if !(1..=100).contains(&objects) || bytes.0 == 0 || bytes.0 > 128 * 1024 * 1024 {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "preview move batch must contain 1–100 objects and at most 128 MiB",
                ));
            }
            service
                .relocation_step(tier, objects.into(), bytes.0)
                .map_err(native)?;
        }
    }
    let config = service.cache_configuration();
    let relocation = service.cache_relocation_snapshot().map_err(native)?;
    let epoch = storage_epoch(catalog).map_err(native)?;
    let stored = service.original_root_review().map_err(native)?;
    if state
        .review
        .as_ref()
        .is_some_and(|review| review.storage_epoch != epoch)
    {
        state.review = None;
    }
    let original_roots = if let Some(review) = &state.review {
        OriginalRoots {
            state: if review.issue.is_some() {
                "blocked"
            } else {
                "reviewing"
            }
            .into(),
            roots: review
                .roots
                .iter()
                .map(|path| NativePath::from_path(path))
                .collect(),
            review: Some(review.id.clone()),
            checked_directories: U64(review.checked),
            uncovered: review.uncovered.clone(),
            message: review.issue.clone().or_else(|| {
                review.projecting.then(|| {
                    "Preparing the bounded original-location index before checking folders.".into()
                })
            }),
        }
    } else {
        let ready = !stored.roots.is_empty() && stored.storage_epoch == Some(epoch);
        let state_name = if stored.roots.is_empty() {
            "required"
        } else if ready {
            "ready"
        } else {
            "stale"
        };
        OriginalRoots {
            state: state_name.into(),
            roots: stored.roots.iter().map(|path| NativePath::from_path(path)).collect(),
            review: None,
            checked_directories: U64(0),
            uncovered: None,
            message: (!ready).then(|| {
                if stored.roots.is_empty() {
                    "Choose and verify every folder boundary that contains original photos before moving previews."
                } else {
                    "Original locations changed since this review. Verify the saved roots again before moving previews."
                }
                .into()
            }),
        }
    };
    Ok(Status {
        thumbnail_root: NativePath::from_path(&config.thumbnail_root),
        large_root: NativePath::from_path(&config.large_root),
        thumbnail_bytes: U64(config.thumbnail_bytes),
        large_bytes: U64(config.large_bytes),
        relocation_pending: relocation.is_some(),
        relocation_tier: relocation.as_ref().map(|r| r.tier),
        relocation: relocation.map(|r| Relocation {
            source: NativePath::from_path(&r.source),
            destination: NativePath::from_path(&r.target),
            phase: r.phase,
            objects: r.objects.map(U64),
            bytes: r.bytes.map(U64),
            total_objects: r.total_objects.map(U64),
            total_bytes: r.total_bytes.map(U64),
        }),
        original_roots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::{Layout, PreviewPolicy, ServiceLimits, StoreConfig};
    use rusqlite::params;

    fn config(root: &std::path::Path) -> StoreConfig {
        StoreConfig {
            manifest_root: root.join("manifest"),
            thumbnail_root: root.join("thumbs"),
            large_root: root.join("large"),
            layout: Layout::HashPrefix,
            thumbnail_bytes: 1024 * 1024,
            large_bytes: 1024 * 1024,
        }
    }
    fn open_service(config: StoreConfig) -> anyhow::Result<PreviewService> {
        PreviewService::open(
            config,
            &[],
            std::env::current_exe()?,
            PreviewPolicy::default(),
            ServiceLimits::default(),
        )
    }
    fn add_original(catalog: &mut Catalog, id: &str, path: &std::path::Path) -> anyhow::Result<()> {
        catalog.db.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES(?1,?2,?3,'pending')",
            params![id, crate::location_bytes(path), path.to_string_lossy()],
        )?;
        catalog.record_storage_path(id, &NativePath::from_path(path))?;
        Ok(())
    }
    fn complete_root_review(
        catalog: &mut Catalog,
        service: &mut PreviewService,
        state: &mut State,
        roots: &[PathBuf],
    ) -> anyhow::Result<Status> {
        let mut status = execute(
            catalog,
            service,
            state,
            Request::BeginOriginalRootReview {
                roots: roots
                    .iter()
                    .map(|path| NativePath::from_path(path))
                    .collect(),
            },
        )?;
        for _ in 0..20 {
            if status.original_roots.state == "ready" {
                return Ok(status);
            }
            let review = status.original_roots.review.clone().context("review id")?;
            status = execute(
                catalog,
                service,
                state,
                Request::StepOriginalRootReview {
                    review,
                    directories: 100,
                },
            )?;
        }
        anyhow::bail!("root review did not complete")
    }
    #[test]
    fn budgets_survive_reopen_without_rounding_and_invalid_update_is_atomic() -> anyhow::Result<()>
    {
        let temp = tempfile::tempdir()?;
        let config = config(temp.path());
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        let mut service = open_service(config.clone())?;
        let mut state = State::default();
        let bytes = 9_007_199_254_740_993;
        execute(
            &mut catalog,
            &mut service,
            &mut state,
            Request::SetBudgets {
                thumbnail_bytes: U64(bytes),
                large_bytes: U64(12345),
            },
        )
        .unwrap();
        assert!(
            execute(
                &mut catalog,
                &mut service,
                &mut state,
                Request::SetBudgets {
                    thumbnail_bytes: U64(0),
                    large_bytes: U64(7)
                },
            )
            .is_err()
        );
        drop(service);
        let mut service = open_service(config)?;
        let status = execute(&mut catalog, &mut service, &mut state, Request::Status).unwrap();
        assert_eq!(status.thumbnail_bytes, U64(bytes));
        assert_eq!(status.large_bytes, U64(12345));
        let encoded = serde_json::to_string(&status)?;
        assert!(encoded.contains("\"9007199254740993\""));
        Ok(())
    }
    #[test]
    fn move_reopens_paused_and_rejects_occupied_destination() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let config = config(temp.path());
        let originals = temp.path().join("originals");
        std::fs::create_dir(&originals)?;
        let occupied = temp.path().join("occupied");
        std::fs::create_dir(&occupied)?;
        std::fs::write(occupied.join("original.jpg"), b"unchanged")?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        let mut service = open_service(config.clone())?;
        let mut state = State::default();
        complete_root_review(
            &mut catalog,
            &mut service,
            &mut state,
            std::slice::from_ref(&originals),
        )?;
        assert!(
            execute(
                &mut catalog,
                &mut service,
                &mut state,
                Request::BeginRelocation {
                    tier: Tier::Large,
                    destination: NativePath::from_path(&occupied)
                },
            )
            .is_err()
        );
        assert_eq!(std::fs::read(occupied.join("original.jpg"))?, b"unchanged");
        let target = temp.path().join("moved");
        let status = execute(
            &mut catalog,
            &mut service,
            &mut state,
            Request::BeginRelocation {
                tier: Tier::Large,
                destination: NativePath::from_path(&target),
            },
        )
        .unwrap();
        assert!(status.relocation_pending);
        drop(service);
        let mut service = open_service(config)?;
        let mut state = State::default();
        assert!(
            execute(&mut catalog, &mut service, &mut state, Request::Status)
                .unwrap()
                .relocation_pending
        );
        for _ in 0..10 {
            let status = execute(
                &mut catalog,
                &mut service,
                &mut state,
                Request::StepRelocation {
                    tier: Tier::Large,
                    objects: 1,
                    bytes: U64(1024),
                },
            )
            .unwrap();
            if !status.relocation_pending {
                assert_eq!(
                    status.large_root,
                    NativePath::from_path(&target.canonicalize()?)
                );
                return Ok(());
            }
        }
        anyhow::bail!("empty relocation did not complete")
    }
    #[test]
    fn oversized_move_batch_is_rejected_without_creating_a_move() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        let mut service = open_service(config(temp.path()))?;
        let mut state = State::default();
        for (objects, bytes) in [(0, 1024), (101, 1024), (1, 0), (1, 134217729)] {
            assert!(
                execute(
                    &mut catalog,
                    &mut service,
                    &mut state,
                    Request::StepRelocation {
                        tier: Tier::Large,
                        objects,
                        bytes: U64(bytes)
                    },
                )
                .is_err()
            );
        }
        assert!(!service.cache_relocation_pending()?);
        Ok(())
    }

    #[test]
    fn relocation_requires_reviewed_roots_and_rejects_a_sibling_inside_them() -> anyhow::Result<()>
    {
        let temp = tempfile::tempdir()?;
        let originals = temp.path().join("originals");
        let year = originals.join("2024");
        std::fs::create_dir_all(&year)?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        add_original(&mut catalog, "photo", &year.join("photo.cr2"))?;
        let mut service = open_service(config(&temp.path().join("cache")))?;
        let mut state = State::default();
        let target = originals.join("LensWorks Previews");
        assert!(
            execute(
                &mut catalog,
                &mut service,
                &mut state,
                Request::BeginRelocation {
                    tier: Tier::Large,
                    destination: NativePath::from_path(&target)
                },
            )
            .unwrap_err()
            .message
            .contains("review every original-photo root")
        );
        complete_root_review(
            &mut catalog,
            &mut service,
            &mut state,
            std::slice::from_ref(&originals),
        )?;
        assert!(
            execute(
                &mut catalog,
                &mut service,
                &mut state,
                Request::BeginRelocation {
                    tier: Tier::Large,
                    destination: NativePath::from_path(&target)
                },
            )
            .is_err()
        );
        assert!(!target.exists());
        Ok(())
    }

    #[test]
    fn root_review_is_bounded_and_storage_changes_make_it_stale() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let originals = temp.path().join("originals");
        std::fs::create_dir_all(originals.join("a"))?;
        std::fs::create_dir_all(originals.join("b"))?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        add_original(&mut catalog, "a", &originals.join("a/photo.jpg"))?;
        add_original(&mut catalog, "b", &originals.join("b/photo.jpg"))?;
        let mut service = open_service(config(&temp.path().join("cache")))?;
        let mut state = State::default();
        let narrow = execute(
            &mut catalog,
            &mut service,
            &mut state,
            Request::BeginOriginalRootReview {
                roots: vec![NativePath::from_path(&originals.join("a"))],
            },
        )?;
        let narrow = execute(
            &mut catalog,
            &mut service,
            &mut state,
            Request::StepOriginalRootReview {
                review: narrow.original_roots.review.context("narrow review")?,
                directories: 10,
            },
        )?;
        assert_eq!(narrow.original_roots.state, "blocked");
        assert_eq!(
            narrow.original_roots.uncovered,
            Some(NativePath::from_path(&originals.join("b")))
        );
        let status = execute(
            &mut catalog,
            &mut service,
            &mut state,
            Request::BeginOriginalRootReview {
                roots: vec![NativePath::from_path(&originals)],
            },
        )?;
        let status = execute(
            &mut catalog,
            &mut service,
            &mut state,
            Request::StepOriginalRootReview {
                review: status.original_roots.review.context("review")?,
                directories: 1,
            },
        )?;
        assert_eq!(status.original_roots.state, "reviewing");
        assert_eq!(status.original_roots.checked_directories, U64(1));
        let ready = complete_root_review(
            &mut catalog,
            &mut service,
            &mut state,
            std::slice::from_ref(&originals),
        )?;
        assert_eq!(ready.original_roots.state, "ready");
        add_original(&mut catalog, "c", &originals.join("a/another.jpg"))?;
        let stale = execute(&mut catalog, &mut service, &mut state, Request::Status)?;
        assert_eq!(stale.original_roots.state, "stale");
        assert!(
            execute(
                &mut catalog,
                &mut service,
                &mut state,
                Request::BeginRelocation {
                    tier: Tier::Large,
                    destination: NativePath::from_path(&temp.path().join("moved"))
                },
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn completed_root_review_survives_catalog_and_manifest_reopen() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let originals = temp.path().join("originals");
        std::fs::create_dir(&originals)?;
        let catalog_root = temp.path().join("catalog");
        let store_config = config(&temp.path().join("cache"));
        let mut catalog = Catalog::open(&catalog_root)?;
        let mut service = open_service(store_config.clone())?;
        let mut state = State::default();
        complete_root_review(
            &mut catalog,
            &mut service,
            &mut state,
            std::slice::from_ref(&originals),
        )?;
        drop(service);
        drop(catalog);
        let mut catalog = Catalog::open(&catalog_root)?;
        let mut service = open_service(store_config)?;
        let mut state = State::default();
        let status = execute(&mut catalog, &mut service, &mut state, Request::Status)?;
        assert_eq!(status.original_roots.state, "ready");
        assert_eq!(
            status.original_roots.roots,
            vec![NativePath::from_path(&originals.canonicalize()?)]
        );
        Ok(())
    }

    #[test]
    fn cross_cache_root_review_requires_live_admission_before_becoming_ready() -> anyhow::Result<()>
    {
        let temp = tempfile::tempdir()?;
        let originals = temp.path().join("originals");
        std::fs::create_dir(&originals)?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        add_original(&mut catalog, "photo", &originals.join("photo.jpg"))?;

        let mut first_service = open_service(config(&temp.path().join("first-cache")))?;
        let mut first_state = State::default();
        let first = complete_root_review(
            &mut catalog,
            &mut first_service,
            &mut first_state,
            std::slice::from_ref(&originals),
        )?;
        assert_eq!(first.original_roots.state, "ready");
        drop(first_service);

        let mut service = open_service(config(&temp.path().join("fresh-cache")))?;
        let mut state = State::default();
        assert!(service.original_root_review()?.roots.is_empty());
        let begun = execute(
            &mut catalog,
            &mut service,
            &mut state,
            Request::BeginOriginalRootReview {
                roots: vec![NativePath::from_path(&originals)],
            },
        )?;
        let review = begun.original_roots.review.context("review")?;

        let error = execute_with_root_admission(
            &mut catalog,
            &mut service,
            &mut state,
            Request::StepOriginalRootReview {
                review: review.clone(),
                directories: 10,
            },
            &mut |_| Err(error(ErrorCode::Native, "injected F admission refusal")),
        )
        .unwrap_err();
        assert!(error.message.contains("injected F admission refusal"));
        assert!(service.original_root_review()?.roots.is_empty());

        let mut admitted = Vec::new();
        let ready = execute_with_root_admission(
            &mut catalog,
            &mut service,
            &mut state,
            Request::StepOriginalRootReview {
                review,
                directories: 10,
            },
            &mut |roots| {
                admitted = roots.to_vec();
                Ok(())
            },
        )?;
        assert_eq!(ready.original_roots.state, "ready");
        assert_eq!(admitted, vec![originals.canonicalize()?]);
        assert_eq!(service.original_root_review()?.roots, admitted);
        Ok(())
    }

    #[test]
    fn root_review_does_not_persist_when_storage_changes_during_live_admission()
    -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let originals = temp.path().join("originals");
        std::fs::create_dir(&originals)?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        add_original(&mut catalog, "photo", &originals.join("photo.jpg"))?;
        let mut service = open_service(config(&temp.path().join("fresh-cache")))?;
        let mut state = State::default();
        let begun = execute(
            &mut catalog,
            &mut service,
            &mut state,
            Request::BeginOriginalRootReview {
                roots: vec![NativePath::from_path(&originals)],
            },
        )?;
        let review = begun.original_roots.review.context("review")?;
        let epoch_writer = rusqlite::Connection::open(catalog.root.join("catalog.sqlite3"))?;
        let error = execute_with_root_admission(
            &mut catalog,
            &mut service,
            &mut state,
            Request::StepOriginalRootReview {
                review,
                directories: 10,
            },
            &mut |_| {
                // Model a catalog actor rotation between coverage and F admission.
                epoch_writer
                    .execute(
                        "UPDATE storage_epoch SET revision=revision+1 WHERE id=1",
                        [],
                    )
                    .map_err(|error| native(error.into()))?;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.message.contains("original locations changed"));
        assert!(service.original_root_review()?.roots.is_empty());
        Ok(())
    }
}
