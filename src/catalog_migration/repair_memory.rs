//! LM import and repair admission for SQLite-owned values without inherited byte limits.
//! Ordinary in-process callers have no installed ledger and preserve their APIs.
//! A phase retains charges until all temporary query/interpretation owners have
//! retired. Values that cross phases (validated binding/progress, Prepared and
//! final Output) are separately included in core::worker_repair_execution.
use crate::lightroom_migration_worker::memory::{
    MemoryBudget, Reservation, core,
    layout::{add, mul},
};
#[cfg(test)]
use anyhow::Context;
use anyhow::{Result, ensure};
use rusqlite::{
    Row, RowIndex,
    types::{FromSql, ValueRef},
};
use std::{cell::RefCell, rc::Rc};

struct State {
    reservation: Reservation,
    floor: usize,
    high: usize,
    live: usize,
    depth: usize,
}
thread_local! {
    static ACTIVE: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
}
pub(crate) struct Operation {
    state: Rc<RefCell<State>>,
}
pub(crate) struct Phase {
    state: Option<Rc<RefCell<State>>>,
}
impl Operation {
    /// Declared before all prepared owners and retained until Output publication.
    pub(crate) fn install(budget: &MemoryBudget) -> Result<Self> {
        let mut reservation = budget.reservation();
        let floor = core::worker_repair_execution()?;
        reservation.grow(floor)?;
        let state = Rc::new(RefCell::new(State {
            reservation,
            floor,
            high: floor,
            live: 0,
            depth: 0,
        }));
        ACTIVE.with(|active| {
            ensure!(
                active.borrow().is_none(),
                "nested managed migration operation"
            );
            *active.borrow_mut() = Some(state.clone());
            Ok(())
        })?;
        Ok(Self { state })
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            active.borrow_mut().take();
        });
        debug_assert_eq!(self.state.borrow().depth, 0);
    }
}
pub(crate) fn phase() -> Phase {
    let state = ACTIVE.with(|active| active.borrow().clone());
    if let Some(state) = &state {
        state.borrow_mut().depth += 1;
    }
    Phase { state }
}
impl Drop for Phase {
    fn drop(&mut self) {
        if let Some(state) = &self.state {
            let mut state = state.borrow_mut();
            state.depth -= 1;
            if state.depth == 0 {
                // Only query scalars and working parser graphs retire here.
                // The separate fixed execution floor still covers returned
                // bounded progress and the next step's overlapping before value.
                state.live = 0;
            }
        }
    }
}
fn admit(bytes: usize) -> Result<()> {
    ACTIVE.with(|active| {
        let state = active.borrow().clone();
        if let Some(state) = state {
            let mut state = state.borrow_mut();
            ensure!(
                state.depth != 0,
                "migration materialization outside an admitted phase"
            );
            let live = add(state.live, bytes)?;
            let required = add(state.floor, live)?;
            state.reservation.ensure_at_least(required)?;
            state.high = state.high.max(required);
            state.live = live;
        }
        Ok(())
    })
}
/// Read the exact statement-owned value before rusqlite creates a String/Vec.
/// No separate length query can race this materialization. Retain the copy and
/// two possible request/receipt clones until this bounded repair step finishes;
/// JSON encoders and typed graphs have separate execution-floor allowances.
pub(crate) fn get<I: RowIndex + Clone, T: FromSql>(row: &Row<'_>, index: I) -> rusqlite::Result<T> {
    if ACTIVE.with(|active| active.borrow().is_some()) {
        let value = row.get_ref(index.clone())?;
        let bytes = match value {
            ValueRef::Text(v) | ValueRef::Blob(v) => v.len(),
            _ => 0,
        };
        if bytes != 0 {
            admit(add(mul(3, bytes).map_err(sql_error)?, 8).map_err(sql_error)?)
                .map_err(sql_error)?;
        }
    }
    row.get(index)
}
fn sql_error(error: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        match error.downcast::<crate::lightroom_migration_worker::memory::ResourceLimit>() {
            Ok(limit) => Box::new(limit),
            Err(error) => error.into_boxed_dyn_error(),
        },
    )
}
/// CatalogData can duplicate ancestor paths in every Property before its final
/// output bound is checked. Admit from the actual borrowed input first.
pub(crate) fn adobe(bytes: &[u8]) -> Result<()> {
    if ACTIVE.with(|active| active.borrow().is_some()) {
        let allocation = core::repair_adobe_catalog(bytes.len())?;
        #[cfg(test)]
        let delta = ACTIVE.with(|active| -> Result<usize> {
            let active = active.borrow();
            let state = active.as_ref().unwrap().borrow();
            Ok(add(add(state.floor, state.live)?, allocation)?.saturating_sub(state.high))
        })?;
        #[cfg(test)]
        observe_adobe(false, delta, bytes.len())?;
        admit(allocation)?;
        #[cfg(test)]
        observe_adobe(true, delta, bytes.len())?;
    }
    Ok(())
}

// Observes the real reservation call; it cannot grant memory or bypass parsing.
// The Unix child fixture forwards these observations as ordinary Progress frames.
#[cfg(test)]
type AdobeCallback = Box<dyn Fn(bool, usize, usize) -> Result<()>>;
#[cfg(test)]
thread_local! {
    static ADOBE_OBSERVER: RefCell<Option<AdobeCallback>> = const { RefCell::new(None) };
}
#[cfg(test)]
pub(crate) struct AdobeObserver;
#[cfg(test)]
impl Drop for AdobeObserver {
    fn drop(&mut self) {
        ADOBE_OBSERVER.with(|observer| {
            observer.borrow_mut().take();
        });
    }
}
#[cfg(test)]
pub(crate) fn install_adobe_observer(callback: AdobeCallback) -> Result<AdobeObserver> {
    ADOBE_OBSERVER.with(|observer| {
        ensure!(
            observer.borrow().is_none(),
            "Adobe admission observer already installed"
        );
        *observer.borrow_mut() = Some(callback);
        Ok(AdobeObserver)
    })
}
#[cfg(test)]
fn observe_adobe(admitted: bool, delta: usize, bytes: usize) -> Result<()> {
    ADOBE_OBSERVER.with(|observer| {
        if let Some(callback) = observer.borrow().as_ref() {
            callback(admitted, delta, bytes)?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lightroom_migration_worker::memory::ResourceLimit;
    use rusqlite::{
        Connection,
        types::{FromSqlError, FromSqlResult},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    static MATERIALIZED: AtomicUsize = AtomicUsize::new(0);
    struct Counted(String);
    impl FromSql for Counted {
        fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
            let ValueRef::Text(value) = value else {
                return Err(FromSqlError::InvalidType);
            };
            MATERIALIZED.fetch_add(1, Ordering::SeqCst);
            Ok(Self(
                std::str::from_utf8(value)
                    .map_err(|e| FromSqlError::Other(Box::new(e)))?
                    .into(),
            ))
        }
    }
    #[test]
    fn lm_executor_batch3_sql_value_denied_before_copy_retries_with_live_sibling() -> Result<()> {
        let db = Connection::open_in_memory()?;
        let value = "é".repeat(8192);
        db.execute_batch("CREATE TABLE stored(a TEXT,b TEXT)")?;
        db.execute("INSERT INTO stored VALUES(?1,?1)", [&value])?;
        let floor = core::worker_repair_execution()?;
        let one = add(mul(3, value.len())?, 8)?;
        let budget = MemoryBudget::new(add(floor, mul(2, one)?)?)?;
        let mut competitor = budget.reservation();
        competitor.grow(1)?;
        let operation = Operation::install(&budget)?;
        {
            let _phase = phase();
            let mut query = db.prepare("SELECT a,b FROM stored")?;
            let mut rows = query.query([])?;
            let row = rows.next()?.unwrap();
            MATERIALIZED.store(0, Ordering::SeqCst);
            let first: Counted = get(row, 0)?;
            let error = match get::<_, Counted>(row, 1) {
                Ok(_) => anyhow::bail!("unadmitted value copied"),
                Err(error) => error,
            };
            assert_eq!(MATERIALIZED.load(Ordering::SeqCst), 1);
            let rusqlite::Error::FromSqlConversionFailure(_, _, cause) = error else {
                anyhow::bail!("wrong failure")
            };
            let limit = cause
                .downcast_ref::<ResourceLimit>()
                .context("missing typed resource cause")?;
            assert_eq!((limit.required, limit.available), (one, one - 1));
            drop(competitor);
            let second: Counted = get(row, 1)?;
            assert_eq!(first.0, second.0);
            assert_eq!(MATERIALIZED.load(Ordering::SeqCst), 2);
        }
        assert_eq!(budget.used(), add(floor, mul(2, one)?)?);
        {
            let _phase = phase();
            let _: String = db.query_row("SELECT a FROM stored", [], |r| get(r, 0))?;
        }
        assert_eq!(budget.used(), add(floor, mul(2, one)?)?);
        drop(operation);
        assert_eq!(budget.used(), 0);
        Ok(())
    }
    #[test]
    fn lm_executor_batch3_stored_policy_and_outcome_shapes_have_independent_admission() -> Result<()>
    {
        use crate::{
            catalog_migration::importer::{self, Outcome, Policy, Progress, Stage},
            storage_volume::NativePath,
        };
        let mut policy = Policy {
            import_source: "legacy".into(),
            overlap: importer::OverlapPolicy::RequireDecision,
            keyword_overlap: importer::KeywordOverlap::RequireDecision,
            artifacts: Vec::new(),
            supplements: Vec::new(),
        };
        for index in 0..1024 {
            policy.artifacts.push(importer::ArtifactInput {
                capture_revision: "a".repeat(64),
                member_index: index,
                mapping: super::super::artifacts::ArtifactMapping {
                    root: NativePath::UnixBytes(vec![b'/'; 1024]),
                    relative: NativePath::UnixBytes(vec![b'a'; 1024]),
                    copy_identity: crate::lightroom::migration_source::FileIdentity {
                        object: "object".into(),
                        bytes: 0,
                        modified_ns: None,
                        changed: "changed".into(),
                    },
                },
            });
        }
        let input = "b".repeat(64);
        let canonical = crate::lightroom::bounded_json(
            &("lightroom-selected-import-v2", &input, &policy),
            8 * 1024 * 1024,
        )?;
        let id = blake3::hash(&canonical).to_hex().to_string();
        let progress = Progress {
            id: id.clone(),
            input,
            stage: Stage::Complete,
            capture_index: 0,
            artifact_index: 0,
            cursor: None,
            processed: 0,
            complete: true,
        };
        let mut raw = serde_json::to_vec(&policy)?;
        raw.extend(std::iter::repeat_n(b' ', 1024 * 1024));
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE migration_retention(id TEXT PRIMARY KEY)")?;
        db.execute(
            "INSERT INTO migration_retention VALUES(?)",
            [&progress.input],
        )?;
        importer::install(&db)?;
        db.execute(
            "INSERT INTO migration_runs VALUES(?1,?2,?3,?4)",
            rusqlite::params![id, progress.input, raw, serde_json::to_vec(&progress)?],
        )?;
        assert!(raw.len() <= 8 * 1024 * 1024);
        // Retained binding bytes need not be canonical JSON. Their exact raw
        // digest is the repair ID, so whitespace remains accepted and charged.
        let mut binding = serde_json::to_vec(&serde_json::json!({
            "adapter":"lightroom-current-container-repair-v1",
            "request":{"run":id,"expected_complete_progress_blake3":"c".repeat(64),
                "expected_mapping_epoch":0,"reason":"r".repeat(4096)},
            "input":progress.input,"policy_blake3":"d".repeat(64)
        }))?;
        binding.extend(std::iter::repeat_n(b' ', 512 * 1024));
        let repair_id = blake3::hash(&binding).to_hex().to_string();
        let repaired = super::super::current_repair::Progress {
            id: repair_id.clone(),
            run: id.clone(),
            input: progress.input.clone(),
            phase: super::super::current_repair::Phase::Complete,
            report_index: 0,
            after_record: 0,
            examined: 0,
            repaired: 0,
            unchanged: 0,
            complete: true,
        };
        let repaired_raw = serde_json::to_vec(&repaired)?;
        let scalar_room = add(mul(3, add(binding.len(), repaired_raw.len())?)?, 16)?;
        db.execute_batch("CREATE TABLE migration_current_repairs(id TEXT PRIMARY KEY,binding BLOB,progress BLOB)")?;
        db.execute(
            "INSERT INTO migration_current_repairs VALUES(?1,?2,?3)",
            rusqlite::params![repair_id, binding, repaired_raw],
        )?;
        let floor = core::worker_repair_execution()?;
        let budget = MemoryBudget::new(add(floor, scalar_room)?)?;
        let mut competitor = budget.reservation();
        competitor.grow(add(scalar_room, 1)?)?;
        assert!(Operation::install(&budget).is_err());
        drop(competitor);
        let operation = Operation::install(&budget)?;
        {
            let _phase = phase();
            let (actual, policy) = importer::read(&db, &id)?;
            assert!(actual.complete);
            assert_eq!(policy.artifacts.len(), 1024);
            assert!(super::super::current_repair::read_progress(&db, &repair_id)?.complete);
            let raw = serde_json::to_vec(&Outcome::Retained {
                reason: "é".repeat(512 * 1024),
            })?;
            let decoded: Outcome = serde_json::from_slice(&raw)?;
            assert!(matches!(decoded,Outcome::Retained {reason} if reason.len()==1024*1024));
        }
        assert_eq!(budget.used(), add(floor, scalar_room)?);
        drop(operation);
        assert_eq!(budget.used(), 0);
        Ok(())
    }
}
