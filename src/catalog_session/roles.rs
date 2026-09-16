use super::{CatalogFilesystem, PhysicalObjectId, RootCapability, SqlRole};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
};

pub(crate) const DISCOVERY: usize = 8;
pub(crate) fn index(role: SqlRole) -> usize {
    match role {
        SqlRole::Actor => 0,
        SqlRole::Relink => 1,
        SqlRole::Export => 2,
        SqlRole::Search0 => 3,
        SqlRole::Search1 => 4,
        SqlRole::Search2 => 5,
        SqlRole::Search3 => 6,
        SqlRole::Manifest => 7,
    }
}
enum Slot {
    Idle(Connection),
    Leased,
    Returned(Connection),
    UnjoinedQuarantine(Connection),
    Quarantined(Connection),
    Closed,
}
enum OpaqueOwner {
    Empty,
    Preparing,
    Held { _owner: Arc<dyn Send + Sync> },
}
struct State {
    slots: Vec<Slot>,
    opaque: Vec<OpaqueOwner>,
    unverifiable_hook_owner: bool,
    closing: bool,
    poisoned: bool,
    released: bool,
}
pub(crate) struct RolePool {
    state: Mutex<State>,
    expected: [PhysicalObjectId; 8],
    filesystem: Arc<dyn CatalogFilesystem>,
    root: RootCapability,
}
impl RolePool {
    pub(crate) fn new(
        connections: Vec<Connection>,
        expected: [PhysicalObjectId; 8],
        filesystem: Arc<dyn CatalogFilesystem>,
        root: RootCapability,
    ) -> Arc<Self> {
        assert_eq!(connections.len(), 9);
        Arc::new(Self {
            state: Mutex::new(State {
                slots: connections.into_iter().map(Slot::Idle).collect(),
                opaque: (0..9).map(|_| OpaqueOwner::Empty).collect(),
                unverifiable_hook_owner: false,
                closing: false,
                poisoned: false,
                released: false,
            }),
            expected,
            filesystem,
            root,
        })
    }
    pub(crate) fn lease(self: &Arc<Self>, index: usize) -> Result<SqlConnection> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            !state.closing && !state.poisoned,
            "catalog SQL owner is closing or poisoned"
        );
        let slot = state.slots.get_mut(index).context("invalid SQL role")?;
        ensure!(
            matches!(slot, Slot::Idle(_)),
            "catalog SQL role is already owned or awaiting join"
        );
        let Slot::Idle(connection) = std::mem::replace(slot, Slot::Leased) else {
            unreachable!()
        };
        Ok(SqlConnection {
            connection: Some(connection),
            lease: Some((self.clone(), index)),
        })
    }
    fn reserve_owner(self: &Arc<Self>, index: usize) -> Result<OwnerReservation> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            index == 7 && matches!(state.slots[index], Slot::Leased),
            "opaque cache owner requires the leased manifest role"
        );
        ensure!(
            matches!(state.opaque[index], OpaqueOwner::Empty),
            "manifest opaque owner is already admitted"
        );
        state.opaque[index] = OpaqueOwner::Preparing;
        Ok(OwnerReservation {
            pool: self.clone(),
            index,
            complete: false,
        })
    }
    pub(crate) fn lease_search(self: &Arc<Self>) -> Result<(usize, SqlConnection)> {
        for index in 3..7 {
            if let Ok(connection) = self.lease(index) {
                return Ok((index, connection));
            }
        }
        anyhow::bail!("four browsing snapshots are already open or draining")
    }
    fn returned(&self, index: usize, db: Connection) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let healthy = !state.unverifiable_hook_owner
            && !std::thread::panicking()
            && db.is_autocommit()
            && (index == DISCOVERY
                || crate::catalog_storage::verify_database_identity(&db, &self.expected[index])
                    .is_ok());
        if !healthy {
            state.poisoned = true;
        }
        state.slots[index] = if healthy {
            Slot::Returned(db)
        } else {
            Slot::UnjoinedQuarantine(db)
        };
    }
    /// The concrete thread owner calls this only after JoinHandle::join. Main
    /// actor/store roles call it after their non-threaded owner was drained/drop.
    pub(crate) fn joined(&self, index: usize, healthy: bool) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !healthy {
            state.poisoned = true;
        }
        let slot = state.slots.get_mut(index).context("invalid SQL role")?;
        match std::mem::replace(slot, Slot::Closed) {
            Slot::Returned(db) if healthy => *slot = Slot::Idle(db),
            Slot::Returned(db) | Slot::UnjoinedQuarantine(db) => *slot = Slot::Quarantined(db),
            other => *slot = other,
        }
        ensure!(
            !matches!(slot, Slot::Leased),
            "SQL worker joined without returning its role"
        );
        ensure!(!state.poisoned, "catalog SQL owner was poisoned");
        Ok(())
    }
    pub(crate) fn is_open(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        !state.closing && !state.poisoned
    }
    pub(crate) fn is_poisoned(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .poisoned
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .released
    }
    pub(crate) fn begin_close(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).closing = true;
    }
    pub(crate) fn close(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closing = true;
        ensure!(
            !state.unverifiable_hook_owner,
            "SQL progress-hook ownership invariant failed; complete owner retained without SQL cleanup"
        );
        ensure!(
            !state.slots.iter().any(|s| matches!(
                s,
                Slot::Leased | Slot::Returned(_) | Slot::UnjoinedQuarantine(_)
            )),
            "catalog SQL roles still owned or awaiting verified join"
        );
        let mut failed = false;
        for slot in &mut state.slots {
            match std::mem::replace(slot, Slot::Closed) {
                Slot::Idle(db) | Slot::Quarantined(db) => {
                    if let Err((db, _)) = db.close() {
                        *slot = Slot::Quarantined(db);
                        failed = true;
                    }
                }
                Slot::Closed => {}
                _ => unreachable!(),
            }
        }
        ensure!(!failed, "SQLite close failed; connection owner retained");
        if !state.released {
            self.filesystem.release_root(&self.root)?;
            state.released = true;
            // All SQL closes and root release are verified. Store failure may
            // have dropped its copy; this was the remaining opaque F lease.
        }
        let owners = std::mem::take(&mut state.opaque);
        drop(state);
        drop(owners);
        Ok(())
    }
}
struct OwnerReservation {
    pool: Arc<RolePool>,
    index: usize,
    complete: bool,
}
impl OwnerReservation {
    fn retain(mut self, owner: Arc<dyn Send + Sync>) {
        self.pool
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .opaque[self.index] = OpaqueOwner::Held { _owner: owner };
        self.complete = true;
    }
}
impl Drop for OwnerReservation {
    fn drop(&mut self) {
        if !self.complete {
            self.pool
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .opaque[self.index] = OpaqueOwner::Empty;
        }
    }
}
impl Drop for RolePool {
    fn drop(&mut self) {
        // Only explicit checked close may release SQL/F ownership. In particular
        // Drop never retries a previously failed or ambiguous drain.
        let state = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        if !state.released {
            std::mem::forget(std::mem::take(&mut state.slots));
            std::mem::forget(std::mem::take(&mut state.opaque));
            std::mem::forget(self.filesystem.clone());
        }
    }
}

/// Keeps business SQL unchanged while returning managed connections without any
/// pathname reopen. Returned does not mean reusable: its thread must be joined.
pub(crate) struct SqlConnection {
    connection: Option<Connection>,
    lease: Option<(Arc<RolePool>, usize)>,
}
impl SqlConnection {
    pub(crate) fn install_cancel_progress(
        &self,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> rusqlite::Result<()> {
        self.progress_hook_result(self.progress_handler(
            1000,
            Some(move || cancel.load(std::sync::atomic::Ordering::Acquire)),
        ))
    }
    pub(crate) fn hook_owner_unverifiable(&self) -> bool {
        self.lease.as_ref().is_some_and(|(pool, _)| {
            pool.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .unverifiable_hook_owner
        })
    }
    pub(crate) fn remove_progress_handler(&self) -> rusqlite::Result<()> {
        self.progress_hook_result(self.progress_handler(0, None::<fn() -> bool>))
    }
    fn progress_hook_result(&self, result: rusqlite::Result<()>) -> rusqlite::Result<()> {
        if result.is_err()
            && let Some((pool, _)) = &self.lease
        {
            let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
            state.unverifiable_hook_owner = true;
            state.poisoned = true;
        }
        result
    }
    #[cfg(test)]
    pub(crate) fn inject_progress_removal_failure(&self) -> rusqlite::Result<()> {
        self.progress_hook_result(Err(rusqlite::Error::InvalidQuery))
    }
    pub(crate) fn retain_opaque_owner(
        &self,
        acquire: impl FnOnce() -> Result<Arc<dyn Send + Sync>>,
    ) -> Result<Arc<dyn Send + Sync>> {
        let (pool, index) = self
            .lease
            .as_ref()
            .context("opaque owner requires managed SQL")?;
        let reservation = pool.reserve_owner(*index)?;
        let owner = acquire()?;
        reservation.retain(owner.clone());
        Ok(owner)
    }
    pub(crate) fn return_managed(&mut self) {
        if let Some((pool, index)) = self.lease.take()
            && let Some(connection) = self.connection.take()
        {
            pool.returned(index, connection);
        }
    }
}
impl From<Connection> for SqlConnection {
    fn from(connection: Connection) -> Self {
        Self {
            connection: Some(connection),
            lease: None,
        }
    }
}
impl Deref for SqlConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.connection.as_ref().expect("owned SQL connection")
    }
}
impl DerefMut for SqlConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        self.connection.as_mut().expect("owned SQL connection")
    }
}
impl Drop for SqlConnection {
    fn drop(&mut self) {
        if let Some((pool, index)) = self.lease.take()
            && let Some(connection) = self.connection.take()
        {
            pool.returned(index, connection);
        }
    }
}
