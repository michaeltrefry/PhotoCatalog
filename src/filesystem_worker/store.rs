//! F-only preview custody. No SQLite, object publication or native execution.
use super::bootstrap::open_directory;
use crate::{
    catalog_session::{CatalogBootstrap, LeaseId, PhysicalObjectId, store::*},
    catalog_storage::physical_object_id,
    preview::{Layout, Tier},
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

type Id = [u8; 16];
fn id(value: &LeaseId) -> Id {
    *uuid::Uuid::parse_str(value.as_str())
        .expect("validated lease ID")
        .as_bytes()
}
fn lease(value: Id) -> LeaseId {
    LeaseId::parse(&uuid::Uuid::from_bytes(value).to_string()).expect("canonical UUID")
}
fn fresh() -> Id {
    *uuid::Uuid::new_v4().as_bytes()
}
fn index(tier: Tier) -> usize {
    if tier == Tier::Thumbnail { 0 } else { 1 }
}
fn check(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "preview ownership operation canceled"
    );
    Ok(())
}

enum NativeUnits {
    Unix(Box<[u8]>),
    Windows(Box<[u16]>),
}
impl NativeUnits {
    fn new(value: NativePath) -> Self {
        match value {
            NativePath::UnixBytes(v) => Self::Unix(v.into_boxed_slice()),
            NativePath::WindowsWide(v) => Self::Windows(v.into_boxed_slice()),
        }
    }
    fn bytes(&self) -> usize {
        match self {
            Self::Unix(v) => v.len(),
            Self::Windows(v) => v.len() * 2,
        }
    }
    fn native(&self) -> NativePath {
        match self {
            Self::Unix(v) => NativePath::UnixBytes(v.to_vec()),
            Self::Windows(v) => NativePath::WindowsWide(v.to_vec()),
        }
    }
    fn path(&self) -> Result<PathBuf> {
        Ok(self.native().to_path()?)
    }
}
struct RootSlot {
    token: Id,
    tier: Tier,
    path: Arc<NativeUnits>,
    marker: Box<[u8]>,
    directory: Option<File>,
    directory_id: Option<PhysicalObjectId>,
    lock: Option<File>,
    lock_id: Option<PhysicalObjectId>,
    stage: Stage,
    retired: bool,
    #[cfg(test)]
    fail_release: bool,
}
impl RootSlot {
    fn facts(&self) -> Lease {
        Lease {
            token: lease(self.token),
            tier: self.tier,
            path: self.path.native(),
        }
    }
    fn bytes(&self) -> usize {
        self.path.bytes()
            + std::mem::size_of::<NativeUnits>()
            + 2 * std::mem::size_of::<usize>()
            + self.marker.len()
    }
    fn verify(&self) -> Result<()> {
        let root = self.path.path()?;
        if let (Some(held), Some(expected)) = (&self.directory, self.directory_id) {
            ensure!(
                physical_object_id(held)? == expected
                    && physical_object_id(&open_directory(&root)?)? == expected,
                "preview root was moved or replaced"
            );
        }
        if let (Some(held), Some(expected)) = (&self.lock, self.lock_id) {
            ensure!(
                physical_object_id(held)? == expected
                    && physical_object_id(&crate::catalog_storage::open_regular(
                        &root.join(".photocatalog-preview-owner")
                    )?)? == expected,
                "preview ownership marker was moved or replaced"
            );
        }
        Ok(())
    }
    fn acquire(&mut self, cancel: &AtomicBool) -> Result<()> {
        if self.stage == Stage::Held {
            return self.verify();
        }
        ensure!(
            self.stage == Stage::Reserved,
            "partial preview acquisition requires final catalog drain"
        );
        check(cancel)?;
        // Record uncertainty before the first side effect. Errors retain this slot.
        self.stage = Stage::Acquiring;
        let root = self.path.path()?;
        fs::create_dir_all(&root)?;
        ensure!(
            fs::canonicalize(&root)? == root,
            "preview root changed during creation"
        );
        let directory = open_directory(&root)?;
        self.directory_id = Some(physical_object_id(&directory)?);
        self.directory = Some(directory);
        check(cancel)?;
        let marker = root.join(".photocatalog-preview-owner");
        if marker.try_exists()? {
            ensure!(
                fs::symlink_metadata(&marker)?.file_type().is_file(),
                "invalid preview ownership marker"
            );
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&marker)?;
        fs2::FileExt::try_lock_exclusive(&file)
            .context("preview location is owned by another process")?;
        self.lock = Some(file); // Ownership is recorded before fallible validation.
        self.lock_id = Some(physical_object_id(self.lock.as_ref().unwrap())?);
        self.verify()?;
        check(cancel)?;
        let file = self.lock.as_mut().unwrap();
        let length = file.metadata()?.len();
        ensure!(
            length <= MARKER_BYTES as u64,
            "invalid preview location identity"
        );
        if length == 0 {
            file.write_all(&self.marker)?;
            file.sync_all()?;
            #[cfg(unix)]
            self.directory.as_ref().unwrap().sync_all()?;
        } else {
            let mut actual = [0; MARKER_BYTES];
            file.read_exact(&mut actual[..length as usize])?;
            let mut extra = [0];
            ensure!(
                file.read(&mut extra)? == 0 && &actual[..length as usize] == self.marker.as_ref(),
                "preview location belongs to another manifest/tier/layout"
            );
        }
        self.verify()?;
        check(cancel)?;
        self.stage = Stage::Held;
        Ok(())
    }
    fn release(&mut self) -> Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_release) {
            anyhow::bail!("injected release failure");
        }
        if let Some(file) = &self.lock {
            fs2::FileExt::unlock(file).context("release preview tier ownership")?;
        }
        self.lock.take();
        self.directory.take();
        self.stage = Stage::Abandoned;
        Ok(())
    }
}
impl Drop for RootSlot {
    fn drop(&mut self) {
        // Unverified destruction is not permission to release an acquired lock.
        if let Some(file) = self.lock.take() {
            std::mem::forget(file);
        }
        if let Some(file) = self.directory.take() {
            std::mem::forget(file);
        }
    }
}
#[derive(Clone, Copy)]
struct Receipt {
    operation: u64,
    digest: [u8; 32],
    stage: Stage,
    value: ReceiptValue,
}
#[derive(Clone, Copy)]
enum ReceiptValue {
    None,
    Acquired,
    Reserved(Id),
    Locked(Id),
    Unit,
}
#[derive(Clone)]
pub(super) struct Snapshot {
    binding: Option<crate::catalog_session::store::StatusQuery>,
    group: Option<Id>,
    current: Option<Receipt>,
    terminal: Option<Receipt>,
    slots: [Option<(Id, Tier, Arc<NativeUnits>)>; ROOT_SLOTS],
    owned_bytes: usize,
}
impl Snapshot {
    pub fn status(&self, query: &StatusQuery) -> Result<Status> {
        query.validate()?;
        let binding = self
            .binding
            .as_ref()
            .context("preview status has no admitted owner")?;
        ensure!(
            binding.epoch == query.epoch
                && binding.token == query.token
                && binding.session == query.session
                && binding.root_physical == query.root_physical
                && binding.catalog_physical == query.catalog_physical,
            "preview status belongs to another catalog session"
        );
        let op = query.operation.0;
        let receipt = self
            .current
            .filter(|r| r.operation == op)
            .or(self.terminal.filter(|r| r.operation == op));
        let selected = query
            .selected
            .as_ref()
            .and_then(|q| self.slots.iter().flatten().find(|s| s.0 == id(q)))
            .map(|(token, tier, path)| Lease {
                token: lease(*token),
                tier: *tier,
                path: path.native(),
            });
        Ok(Status {
            operation: query.operation,
            group: self.group.map(lease),
            stage: receipt.map(|r| r.stage),
            slots: self.slots.iter().flatten().count(),
            owned_bytes: self.owned_bytes,
            selected,
        })
    }
}
pub(super) struct StoreOwner {
    group: Option<Id>,
    identity: Box<str>,
    layout: Layout,
    slots: [Option<RootSlot>; ROOT_SLOTS],
    tiers: [Option<Id>; 2],
    extra: Option<Id>,
    current: Option<Receipt>,
    terminal: Option<Receipt>,
    high_water: u64,
    budget: usize,
    failure: Option<super::wire::Failure>,
}
impl Default for StoreOwner {
    fn default() -> Self {
        Self {
            group: None,
            identity: "".into(),
            layout: Layout::Flat,
            slots: std::array::from_fn(|_| None),
            tiers: [None, None],
            extra: None,
            current: None,
            terminal: None,
            high_water: 0,
            budget: OWNED_BYTES,
            failure: None,
        }
    }
}
impl StoreOwner {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + std::mem::size_of::<Snapshot>()
            + 3 * 36
            + super::wire::ERROR_BYTES
            + self.identity.len()
            + self
                .slots
                .iter()
                .flatten()
                .map(RootSlot::bytes)
                .sum::<usize>()
    }
    fn slot(&self, token: Id) -> Result<&RootSlot> {
        self.slots
            .iter()
            .flatten()
            .find(|s| s.token == token)
            .context("unknown preview root capability")
    }
    fn slot_mut(&mut self, token: Id) -> Result<&mut RootSlot> {
        self.slots
            .iter_mut()
            .flatten()
            .find(|s| s.token == token)
            .context("unknown preview root capability")
    }
    fn group(&self, value: &LeaseId) -> Result<()> {
        ensure!(
            self.group == Some(id(value)),
            "preview group belongs to another admission"
        );
        Ok(())
    }
    fn reserve(
        &mut self,
        root: &NativePath,
        tier: Tier,
        original_roots: &[NativePath],
        manifest: &Path,
    ) -> Result<Id> {
        path(root)?;
        let root_path = root.to_path()?;
        ensure!(
            crate::prospective_directory(&root_path)? == root_path,
            "preview request path is not prospectively resolved"
        );
        let separate = |a: &Path, b: &Path| -> Result<()> {
            ensure!(
                !a.starts_with(b) && !b.starts_with(a),
                "preview storage roots overlap"
            );
            Ok(())
        };
        separate(&root_path, manifest)?;
        for original in original_roots {
            separate(
                &root_path,
                &crate::prospective_directory(&original.to_path()?)?,
            )?;
        }
        for slot in self.slots.iter().flatten() {
            let held = slot.path.path()?;
            if held == root_path {
                ensure!(
                    slot.tier == tier && slot.stage == Stage::Held,
                    "preview root is already reserved or belongs to another tier"
                );
                slot.verify()?;
                return Ok(slot.token);
            }
            separate(&root_path, &held)?;
        }
        let vacant = self.slots.iter().position(Option::is_none).ok_or(ResourceLimit("Preview storage has retained ownership of 16 locations in this catalog session. The current cache is still available. Close this catalog successfully, reopen it, and retry relocation"))?;
        let marker = marker(&self.identity, tier, self.layout)?.into_boxed_slice();
        let units = Arc::new(NativeUnits::new(root.clone()));
        ensure!(
            self.bytes()
                .checked_add(
                    units.bytes()
                        + std::mem::size_of::<NativeUnits>()
                        + 2 * std::mem::size_of::<usize>()
                        + marker.len()
                )
                .is_some_and(|n| n <= self.budget),
            ResourceLimit(
                "Preview storage reached its 2 MiB ownership-metadata allowance. The current cache is still available. Close this catalog successfully, reopen it, and retry relocation"
            )
        );
        let token = fresh();
        self.slots[vacant] = Some(RootSlot {
            token,
            tier,
            path: units,
            marker,
            directory: None,
            directory_id: None,
            lock: None,
            lock_id: None,
            stage: Stage::Reserved,
            retired: false,
            #[cfg(test)]
            fail_release: false,
        });
        Ok(token)
    }
    pub fn execute(
        &mut self,
        bootstrap: &CatalogBootstrap,
        originals: &[NativePath],
        request: &Request,
        cancel: &AtomicBool,
        mut publish: impl FnMut(Snapshot) -> Result<()>,
    ) -> Result<Reply> {
        request.validate()?;
        ensure!(
            request.root == bootstrap.root_capability(),
            "preview request belongs to another catalog session"
        );
        check(cancel)?;
        let encoded = super::wire::encode(request, super::wire::MESSAGE_BYTES)?;
        let digest = *blake3::hash(&encoded).as_bytes();
        let operation = request.operation.0;
        if let Some(previous) = self.current {
            ensure!(
                previous.operation == operation && previous.digest == digest,
                "preview ownership operation requires reconciliation or final drain"
            );
            anyhow::bail!(
                "preview ownership outcome is retained; inspect store status before explicit cleanup"
            );
        }
        if let Some(previous) = self.terminal
            && previous.operation == operation
        {
            ensure!(
                previous.digest == digest,
                "altered preview operation replay"
            );
            if let Some(failure) = &self.failure {
                return Err(failure.clone().into());
            }
            return self.receipt_value(previous.value).map(|value| Reply {
                operation: request.operation,
                value,
            });
        }
        ensure!(
            operation > self.high_water,
            "stale preview operation sequence"
        );
        self.high_water = operation;
        self.failure = None;
        self.current = Some(Receipt {
            operation,
            digest,
            stage: Stage::Acquiring,
            value: ReceiptValue::None,
        });
        publish(self.snapshot(request))?;
        let result = self.execute_inner(
            bootstrap,
            originals,
            &request.action,
            cancel,
            &mut |owner| publish(owner.snapshot(request)),
        );
        let stage = if result.is_ok() {
            Stage::Complete
        } else {
            Stage::Failed
        };
        let receipt = self.current.as_mut().unwrap();
        receipt.stage = stage;
        receipt.value = match &result {
            Ok(Value::Acquired(_)) => ReceiptValue::Acquired,
            Ok(Value::Reserved(v)) => ReceiptValue::Reserved(id(&v.token)),
            Ok(Value::Locked(v)) => ReceiptValue::Locked(id(&v.token)),
            Ok(Value::Unit) => ReceiptValue::Unit,
            Err(error) => {
                let kind = if let Some(failure) = error.downcast_ref::<super::wire::Failure>() {
                    failure.kind
                } else if error.is::<ResourceLimit>() {
                    super::wire::FailureKind::ResourceLimit
                } else {
                    super::wire::FailureKind::Unknown
                };
                self.failure = Some(super::wire::Failure::new(kind, error));
                ReceiptValue::None
            }
        };
        // Effects stay in slots. A terminal receipt is bounded independently of them.
        self.terminal = self.current.take();
        publish(self.snapshot(request))?;
        result.map(|value| Reply {
            operation: request.operation,
            value,
        })
    }
    fn acquired(&self) -> Result<Acquired> {
        Ok(Acquired {
            group: lease(self.group.context("preview group unavailable")?),
            tiers: [
                self.slot(self.tiers[0].context("thumbnail ownership unavailable")?)?
                    .facts(),
                self.slot(self.tiers[1].context("large ownership unavailable")?)?
                    .facts(),
            ],
            extra: self
                .extra
                .map(|token| self.slot(token).map(RootSlot::facts))
                .transpose()?,
        })
    }
    fn receipt_value(&self, value: ReceiptValue) -> Result<Value> {
        Ok(match value {
            ReceiptValue::Acquired => Value::Acquired(self.acquired()?),
            ReceiptValue::Reserved(token) => Value::Reserved(self.slot(token)?.facts()),
            ReceiptValue::Locked(token) => Value::Locked(self.slot(token)?.facts()),
            ReceiptValue::Unit => Value::Unit,
            ReceiptValue::None => anyhow::bail!("preview operation has no completed receipt"),
        })
    }
    fn snapshot(&self, request: &Request) -> Snapshot {
        Snapshot {
            binding: Some(StatusQuery::from(&Query {
                root: request.root.clone(),
                operation: request.operation,
                selected: None,
            })),
            group: self.group,
            current: self.current,
            terminal: self.terminal,
            slots: std::array::from_fn(|n| {
                self.slots[n]
                    .as_ref()
                    .map(|s| (s.token, s.tier, s.path.clone()))
            }),
            owned_bytes: self.bytes(),
        }
    }
    fn execute_inner(
        &mut self,
        bootstrap: &CatalogBootstrap,
        originals: &[NativePath],
        action: &Action,
        cancel: &AtomicBool,
        publish: &mut impl FnMut(&Self) -> Result<()>,
    ) -> Result<Value> {
        let manifest = bootstrap.manifest.path.to_path()?;
        let manifest = manifest.parent().context("manifest has no parent")?;
        match action {
            Action::Acquire(value) => {
                ensure!(
                    self.group.is_none(),
                    "preview store is already admitted or awaiting cleanup"
                );
                value.validate()?;
                ensure!(
                    self.bytes() + value.identity.len() <= self.budget,
                    ResourceLimit("preview ownership metadata budget")
                );
                self.identity = value.identity.clone().into_boxed_str();
                self.layout = value.layout;
                self.group = Some(fresh());
                // Reserve the complete baseline before any root/marker effect.
                for (root, tier) in value.roots.iter().zip([Tier::Thumbnail, Tier::Large]) {
                    self.tiers[index(tier)] = Some(self.reserve(root, tier, originals, manifest)?);
                }
                if let Some(relocation) = &value.relocation {
                    let current = if relocation.cleanup {
                        &relocation.target
                    } else {
                        &relocation.source
                    };
                    ensure!(
                        &value.roots[index(relocation.tier)] == current,
                        "preview relocation does not match current tier location"
                    );
                    let extra = if relocation.cleanup {
                        &relocation.source
                    } else {
                        &relocation.target
                    };
                    self.extra = Some(self.reserve(extra, relocation.tier, originals, manifest)?);
                }
                publish(self)?;
                for n in 0..ROOT_SLOTS {
                    if let Some(slot) = self.slots[n].as_mut() {
                        slot.acquire(cancel)?;
                        publish(self)?;
                    }
                }
                Ok(Value::Acquired(self.acquired()?))
            }
            Action::Reserve {
                group,
                tier,
                destination,
            } => {
                self.group(group)?;
                if let Some(token) = self.extra {
                    let slot = self.slot(token)?;
                    ensure!(
                        slot.tier == *tier
                            && slot.path.native() == *destination
                            && matches!(slot.stage, Stage::Reserved | Stage::Held),
                        "another preview relocation is pending or requires final drain"
                    );
                    return Ok(Value::Reserved(slot.facts()));
                }
                let token = self.reserve(destination, *tier, originals, manifest)?;
                self.extra = Some(token);
                Ok(Value::Reserved(self.slot(token)?.facts()))
            }
            Action::Lock { group, reservation } => {
                self.group(group)?;
                ensure!(
                    self.extra == Some(id(reservation)),
                    "preview reservation is not current"
                );
                publish(self)?;
                let slot = self.slot_mut(id(reservation))?;
                slot.acquire(cancel)?;
                Ok(Value::Locked(slot.facts()))
            }
            Action::Promote {
                group,
                target,
                tier,
            } => {
                self.group(group)?;
                ensure!(
                    self.extra == Some(id(target)),
                    "preview relocation target is not current"
                );
                let target_slot = self.slot(id(target))?;
                ensure!(
                    target_slot.tier == *tier && target_slot.stage == Stage::Held,
                    "preview relocation target is not held"
                );
                target_slot.verify()?;
                let old = self.tiers[index(*tier)].replace(id(target));
                self.extra = old;
                Ok(Value::Unit)
            }
            Action::Retire { group, old } => {
                self.group(group)?;
                if self.extra.is_none() && self.slot(id(old))?.retired {
                    return Ok(Value::Unit);
                }
                ensure!(
                    self.extra == Some(id(old)),
                    "preview cleanup root is not current"
                );
                let slot = self.slot_mut(id(old))?;
                ensure!(
                    slot.stage == Stage::Held,
                    "preview cleanup ownership unavailable"
                );
                slot.retired = true;
                self.extra = None;
                Ok(Value::Unit)
            }
            Action::Abandon { group, reservation } => {
                self.group(group)?;
                let position = self
                    .slots
                    .iter()
                    .position(|s| s.as_ref().is_some_and(|s| s.token == id(reservation)))
                    .context("unknown preview reservation")?;
                ensure!(
                    self.extra == Some(id(reservation))
                        && (self.slots[position].as_ref().unwrap().stage == Stage::Reserved
                            || self.slots[position].as_ref().unwrap().retired),
                    "preview reservation may have effects; final catalog drain required"
                );
                if self.slots[position].as_ref().unwrap().stage == Stage::Reserved {
                    self.slots[position].take();
                }
                self.extra = None;
                Ok(Value::Unit)
            }
        }
    }
    pub fn release(&mut self) -> Result<()> {
        for slot in self.slots.iter_mut().flatten() {
            slot.release()?;
        }
        self.slots = std::array::from_fn(|_| None);
        self.identity = "".into();
        self.group = None;
        Ok(())
    }
}

pub(super) fn read_configuration(path: &NativePath, cancel: &AtomicBool) -> Result<Vec<u8>> {
    crate::catalog_session::store::path(path)?;
    check(cancel)?;
    let mut file = File::open(path.to_path()?).context("open preview configuration")?;
    let length = file.metadata()?.len();
    ensure!(
        length <= CONFIG_BYTES as u64,
        ResourceLimit("preview configuration size limit")
    );
    check(cancel)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length as usize)?;
    bytes.resize(length as usize, 0);
    file.read_exact(&mut bytes)?;
    check(cancel)?;
    let mut extra = [0];
    ensure!(file.read(&mut extra)? == 0, "preview configuration grew");
    check(cancel)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{application::U64, catalog_session::PinnedDatabase};
    struct Fixture {
        directory: tempfile::TempDir,
        bootstrap: CatalogBootstrap,
        owner: StoreOwner,
        next: std::cell::Cell<u64>,
    }
    impl Fixture {
        fn new() -> Result<Self> {
            let directory = tempfile::tempdir()?;
            let root = fs::canonicalize(directory.path())?;
            fs::create_dir(root.join("catalog"))?;
            fs::create_dir(root.join("manifest"))?;
            let catalog = root.join("catalog/catalog.sqlite3");
            let manifest = root.join("manifest/previews.sqlite3");
            let catalog_id = physical_object_id(&File::create(&catalog)?)?;
            let manifest_id = physical_object_id(&File::create(&manifest)?)?;
            let bootstrap = CatalogBootstrap {
                version: 1,
                operation: U64(1),
                epoch: LeaseId::new(),
                token: LeaseId::new(),
                session: LeaseId::new(),
                canonical_root: NativePath::from_path(&root.join("catalog")),
                root_physical: physical_object_id(&open_directory(&root.join("catalog"))?)?,
                catalog: PinnedDatabase {
                    path: NativePath::from_path(&catalog),
                    physical: catalog_id,
                    created: true,
                },
                manifest: PinnedDatabase {
                    path: NativePath::from_path(&manifest),
                    physical: manifest_id,
                    created: true,
                },
            };
            Ok(Self {
                directory,
                bootstrap,
                owner: StoreOwner::default(),
                next: std::cell::Cell::new(1),
            })
        }
        fn path(&self, name: &str) -> NativePath {
            NativePath::from_path(&fs::canonicalize(self.directory.path()).unwrap().join(name))
        }
        fn request(&self, action: Action) -> Request {
            let sequence = self.next.get();
            self.next.set(sequence.checked_add(1).unwrap());
            Request {
                root: self.bootstrap.root_capability(),
                operation: U64(sequence),
                action,
            }
        }
        fn execute(&mut self, request: &Request) -> Result<Reply> {
            self.owner.execute(
                &self.bootstrap,
                &[],
                request,
                &AtomicBool::new(false),
                |_| Ok(()),
            )
        }
        fn acquire(&mut self) -> Result<Acquired> {
            let request = self.request(Action::Acquire(Descriptor {
                identity: "arbitrary non-UUID identity".into(),
                layout: Layout::Flat,
                roots: [self.path("thumb"), self.path("large")],
                relocation: None,
            }));
            let Value::Acquired(value) = self.execute(&request)?.value else {
                unreachable!()
            };
            Ok(value)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.owner.release();
        }
    }
    fn locked(path: &NativePath) -> Result<bool> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.to_path()?.join(".photocatalog-preview-owner"))?;
        let held = fs2::FileExt::try_lock_exclusive(&file).is_err();
        if !held {
            fs2::FileExt::unlock(&file)?;
        }
        Ok(held)
    }
    #[test]
    fn retired_roots_hold_real_locks_until_final_drain_and_slot_exhaustion_is_retryable()
    -> Result<()> {
        let mut f = Fixture::new()?;
        let group = f.acquire()?;
        let first = group.tiers[0].clone();
        let mut current = first.clone();
        for n in 0..ROOT_SLOTS - 2 {
            let request = f.request(Action::Reserve {
                group: group.group.clone(),
                tier: Tier::Thumbnail,
                destination: f.path(&format!("relocated-{n}")),
            });
            let Value::Reserved(target) = f.execute(&request)?.value else {
                unreachable!()
            };
            let lock = f.request(Action::Lock {
                group: group.group.clone(),
                reservation: target.token.clone(),
            });
            f.execute(&lock)?;
            let promote = f.request(Action::Promote {
                group: group.group.clone(),
                target: target.token.clone(),
                tier: Tier::Thumbnail,
            });
            f.execute(&promote)?;
            let retire = f.request(Action::Retire {
                group: group.group.clone(),
                old: current.token,
            });
            f.execute(&retire)?;
            current = target;
        }
        assert!(locked(&first.path)? && locked(&current.path)?);
        let destination = f.path("denied-seventeenth");
        let request = f.request(Action::Reserve {
            group: group.group.clone(),
            tier: Tier::Thumbnail,
            destination: destination.clone(),
        });
        assert!(f.execute(&request).unwrap_err().is::<ResourceLimit>());
        assert!(!destination.to_path()?.exists());
        assert!(locked(&group.tiers[1].path)?);
        f.owner.release()?;
        assert!(!locked(&first.path)? && !locked(&current.path)?);
        // Only an explicit drained session starts with a fresh finite allowance.
        f.owner = StoreOwner::default();
        f.acquire()?;
        Ok(())
    }
    #[test]
    fn private_byte_budget_denies_before_io_and_new_attempt_can_retry() -> Result<()> {
        let mut f = Fixture::new()?;
        let group = f.acquire()?;
        f.owner.budget = f.owner.bytes();
        let destination = f.path("byte-denied");
        let request = f.request(Action::Reserve {
            group: group.group.clone(),
            tier: Tier::Large,
            destination: destination.clone(),
        });
        assert!(f.execute(&request).unwrap_err().is::<ResourceLimit>());
        assert!(!destination.to_path()?.exists());
        assert!(locked(&group.tiers[1].path)?);
        f.owner.budget = OWNED_BYTES;
        let retry = f.request(request.action.clone());
        let Value::Reserved(target) = f.execute(&retry)?.value else {
            unreachable!()
        };
        let abandon = f.request(Action::Abandon {
            group: group.group,
            reservation: target.token,
        });
        f.execute(&abandon)?;
        assert_eq!(f.owner.slots.iter().flatten().count(), 2);
        assert!(!destination.to_path()?.exists());
        Ok(())
    }
    #[test]
    fn exact_operation_replay_and_cached_status_do_not_repeat_effects() -> Result<()> {
        let mut f = Fixture::new()?;
        let group = f.acquire()?;
        let request = f.request(Action::Reserve {
            group: group.group.clone(),
            tier: Tier::Large,
            destination: f.path("target"),
        });
        let first = f.execute(&request)?;
        assert_eq!(first, f.execute(&request)?);
        assert_eq!(f.owner.slots.iter().flatten().count(), 3);
        let Value::Reserved(root) = first.value else {
            unreachable!()
        };
        let query = Query {
            root: request.root.clone(),
            operation: request.operation,
            selected: Some(root.token.clone()),
        };
        let snapshot = f.owner.snapshot(&request);
        let status = snapshot.status(&StatusQuery::from(&query))?;
        assert_eq!(status.selected, Some(root));
        assert_eq!(status.stage, Some(Stage::Complete));
        let mut altered = request.clone();
        altered.action = Action::Retire {
            group: group.group,
            old: LeaseId::new(),
        };
        assert!(f.execute(&altered).is_err());
        let mut foreign = StatusQuery::from(&query);
        foreign.session = LeaseId::new();
        assert!(snapshot.status(&foreign).is_err());
        Ok(())
    }
    #[test]
    fn partial_acquisition_keeps_lock_and_terminal_error_until_explicit_drain() -> Result<()> {
        let mut f = Fixture::new()?;
        let target = f.path("thumb");
        fs::create_dir(target.to_path()?)?;
        fs::write(
            target.to_path()?.join(".photocatalog-preview-owner"),
            b"foreign",
        )?;
        let request = f.request(Action::Acquire(Descriptor {
            identity: "local".into(),
            layout: Layout::Flat,
            roots: [target.clone(), f.path("large")],
            relocation: None,
        }));
        assert!(f.execute(&request).is_err());
        assert!(locked(&target)?);
        assert!(f.execute(&request).is_err());
        assert!(locked(&target)?);
        f.owner.release()?;
        assert!(!locked(&target)?);
        assert_eq!(
            fs::read(target.to_path()?.join(".photocatalog-preview-owner"))?,
            b"foreign"
        );
        Ok(())
    }
    #[test]
    fn canceled_acquisition_and_oversized_identity_have_no_root_effects() -> Result<()> {
        let mut f = Fixture::new()?;
        let mut request = f.request(Action::Acquire(Descriptor {
            identity: "valid".into(),
            layout: Layout::Flat,
            roots: [f.path("thumb"), f.path("large")],
            relocation: None,
        }));
        assert!(
            f.owner
                .execute(&f.bootstrap, &[], &request, &AtomicBool::new(true), |_| Ok(
                    ()
                ))
                .is_err()
        );
        if let Action::Acquire(value) = &mut request.action {
            value.identity = "\\".repeat(256);
        }
        assert!(f.execute(&request).unwrap_err().is::<ResourceLimit>());
        assert!(!f.path("thumb").to_path()?.exists());
        Ok(())
    }
    #[test]
    fn failed_final_release_retains_remaining_ownership_and_explicit_retry_releases() -> Result<()>
    {
        let mut f = Fixture::new()?;
        let group = f.acquire()?;
        f.owner.slot_mut(id(&group.tiers[0].token))?.fail_release = true;
        assert!(f.owner.release().is_err());
        assert!(locked(&group.tiers[0].path)? && locked(&group.tiers[1].path)?);
        f.owner.release()?;
        assert!(!locked(&group.tiers[0].path)? && !locked(&group.tiers[1].path)?);
        Ok(())
    }
    #[test]
    fn recovering_baseline_reserves_three_before_effects_and_supports_cleanup_phase() -> Result<()>
    {
        let mut f = Fixture::new()?;
        let source = f.path("old");
        let target = f.path("new");
        let request = f.request(Action::Acquire(Descriptor {
            identity: "restored manifest identity".into(),
            layout: Layout::Flat,
            roots: [target.clone(), f.path("large")],
            relocation: Some(Relocation {
                id: "saved-relocation".into(),
                tier: Tier::Thumbnail,
                source: source.clone(),
                target: target.clone(),
                cleanup: true,
            }),
        }));
        let Value::Acquired(group) = f.execute(&request)?.value else {
            unreachable!()
        };
        assert_eq!(group.tiers[0].path, target);
        assert_eq!(group.extra.as_ref().unwrap().path, source);
        assert!(locked(&source)? && locked(&target)?);
        let old = group.extra.unwrap();
        let retire = f.request(Action::Retire {
            group: group.group,
            old: old.token,
        });
        f.execute(&retire)?;
        assert!(locked(&source)?);
        Ok(())
    }
    #[test]
    fn capacity_ledger_uses_actual_box_lengths_and_fixed_container_sizes() -> Result<()> {
        let mut f = Fixture::new()?;
        f.acquire()?;
        let dynamic = f.owner.identity.len()
            + f.owner
                .slots
                .iter()
                .flatten()
                .map(|s| {
                    s.path.bytes()
                        + std::mem::size_of::<NativeUnits>()
                        + 2 * std::mem::size_of::<usize>()
                        + s.marker.len()
                })
                .sum::<usize>();
        let fixed = std::mem::size_of::<StoreOwner>()
            + std::mem::size_of::<Snapshot>()
            + 3 * 36
            + super::super::wire::ERROR_BYTES;
        assert_eq!(f.owner.bytes(), fixed + dynamic);
        assert!(f.owner.bytes() < OWNED_BYTES);
        eprintln!(
            "FS6 capacity StoreOwner={} RootSlot={} Snapshot={} NativeUnits={} pointer={} fixed={} current={}",
            std::mem::size_of::<StoreOwner>(),
            std::mem::size_of::<RootSlot>(),
            std::mem::size_of::<Snapshot>(),
            std::mem::size_of::<NativeUnits>(),
            std::mem::size_of::<usize>(),
            fixed,
            f.owner.bytes()
        );
        Ok(())
    }
    #[test]
    fn stale_operation_after_multiple_completions_cannot_change_mappings_or_effects() -> Result<()>
    {
        let mut f = Fixture::new()?;
        let group = f.acquire()?;
        let reserve = f.request(Action::Reserve {
            group: group.group.clone(),
            tier: Tier::Thumbnail,
            destination: f.path("destination"),
        });
        let Value::Reserved(target) = f.execute(&reserve)?.value else {
            unreachable!()
        };
        let lock = f.request(Action::Lock {
            group: group.group.clone(),
            reservation: target.token.clone(),
        });
        f.execute(&lock)?;
        let promote = f.request(Action::Promote {
            group: group.group.clone(),
            tier: Tier::Thumbnail,
            target: target.token.clone(),
        });
        f.execute(&promote)?;
        let retire = f.request(Action::Retire {
            group: group.group,
            old: group.tiers[0].token.clone(),
        });
        f.execute(&retire)?;
        let before = (
            f.owner.tiers,
            f.owner.extra,
            f.owner.bytes(),
            f.owner.high_water,
        );
        assert!(
            f.execute(&reserve)
                .unwrap_err()
                .to_string()
                .contains("stale")
        );
        assert!(f.execute(&lock).unwrap_err().to_string().contains("stale"));
        let mut altered = retire.clone();
        altered.action = reserve.action.clone();
        assert!(
            f.execute(&altered)
                .unwrap_err()
                .to_string()
                .contains("altered")
        );
        assert_eq!(
            (
                f.owner.tiers,
                f.owner.extra,
                f.owner.bytes(),
                f.owner.high_water
            ),
            before
        );
        assert!(locked(&target.path)? && locked(&group.tiers[0].path)?);
        // Cancellation/local admission can consume C IDs without entering F.
        f.next.set(100);
        let gap = f.request(Action::Reserve {
            group: f.owner.acquired()?.group,
            tier: Tier::Large,
            destination: f.path("after-gap"),
        });
        f.execute(&gap)?;
        assert_eq!(f.owner.high_water, 100);
        Ok(())
    }
    #[test]
    fn exact_resource_failure_replay_preserves_filesystem_handler_classification() -> Result<()> {
        let mut f = Fixture::new()?;
        let group = f.acquire()?;
        f.owner.budget = f.owner.bytes();
        let request = f.request(Action::Reserve {
            group: group.group,
            tier: Tier::Large,
            destination: f.path("over-budget"),
        });
        // Exercise the exact classifier used by FilesystemHandler::execute;
        // the replay input is an already typed cached wire::Failure.
        let first = super::super::filesystem_failure(f.execute(&request).unwrap_err());
        let replay = super::super::filesystem_failure(f.execute(&request).unwrap_err());
        assert_eq!(first.kind, super::super::wire::FailureKind::ResourceLimit);
        assert_eq!(replay.kind, first.kind);
        assert_eq!(replay.message, first.message);
        Ok(())
    }
    #[test]
    fn configuration_reader_caps_bytes_and_obeys_cancel() -> Result<()> {
        let f = Fixture::new()?;
        let path = f.path("configuration.json");
        fs::write(path.to_path()?, vec![b' '; CONFIG_BYTES])?;
        assert_eq!(
            read_configuration(&path, &AtomicBool::new(false))?.len(),
            CONFIG_BYTES
        );
        assert!(read_configuration(&path, &AtomicBool::new(true)).is_err());
        fs::write(path.to_path()?, vec![0; CONFIG_BYTES + 1])?;
        assert!(
            read_configuration(&path, &AtomicBool::new(false))
                .unwrap_err()
                .is::<ResourceLimit>()
        );
        Ok(())
    }
}
