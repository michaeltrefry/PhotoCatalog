//! In-process admission to a catalog's SQLite writer. SQLite still arbitrates
//! independent processes. Waiting interactive work takes precedence over queued
//! background transactions; each class is FIFO. An unbounded foreground stream
//! can postpone background work. Parsing/rendering must happen before admission.
use anyhow::{Result, ensure};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, OnceLock, Weak},
    thread::{self, ThreadId},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Priority {
    Foreground,
    Background,
}
#[derive(Default)]
struct State {
    owner: Option<ThreadId>,
    next: [u64; 2],
    serving: [u64; 2],
}
#[derive(Default)]
pub(crate) struct Writers {
    state: Mutex<State>,
    changed: Condvar,
}

pub(crate) fn for_catalog(canonical_root: &Path) -> Arc<Writers> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Writers>>>> = OnceLock::new();
    let mut registry = REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    registry.retain(|_, value| value.strong_count() != 0);
    if let Some(value) = registry.get(canonical_root).and_then(Weak::upgrade) {
        return value;
    }
    let value = Arc::new(Writers::default());
    registry.insert(canonical_root.to_owned(), Arc::downgrade(&value));
    value
}

impl Writers {
    pub(crate) fn enter(self: &Arc<Self>, priority: Priority) -> Result<Permit> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let owner = thread::current().id();
        ensure!(
            state.owner != Some(owner),
            "recursive catalog writer admission; release the current transaction before calling another catalog writer"
        );
        let class = usize::from(priority == Priority::Background);
        let ticket = state.next[class];
        state.next[class] = ticket
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("catalog writer ticket exhausted"))?;
        self.changed.notify_all();
        while state.owner.is_some()
            || state.serving[class] != ticket
            || (class == 1 && state.next[0] != state.serving[0])
        {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.serving[class] += 1;
        state.owner = Some(owner);
        Ok(Permit {
            writers: self.clone(),
            _not_send: std::marker::PhantomData,
        })
    }
}

/// Declared before the transaction so rollback drops before this permit on error.
/// Owned rather than borrowing Catalog, leaving its connection freely mutable.
pub(crate) struct Permit {
    writers: Arc<Writers>,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.writers.state.lock().unwrap_or_else(|e| e.into_inner());
        state.owner = None;
        self.writers.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Catalog, catalog_metadata::Source, organization::Operation, xmp_packets};
    use rusqlite::params;
    use std::{fs, time::Duration};

    fn queued(gate: &Writers, foreground: u64, background: u64) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut state = gate.state.lock().unwrap();
        while state.next[0] - state.serving[0] != foreground
            || state.next[1] - state.serving[1] != background
        {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "writer did not reach admission");
            state = gate.changed.wait_timeout(state, left).unwrap().0;
        }
    }
    fn source() -> Source {
        Source {
            kind: "imported_catalog".into(),
            locator: b"fixture".to_vec(),
            display: "fixture".into(),
            ambiguous: false,
            provenance: serde_json::json!({"test":true}),
        }
    }
    fn fixture() -> Result<(tempfile::TempDir, Catalog, PathBuf)> {
        let temp = tempfile::tempdir()?;
        let mut cat = Catalog::open(temp.path().join("catalog"))?;
        for asset in ["background1", "background2", "foreground1", "foreground2"] {
            cat.db.execute(
                "INSERT INTO assets(id,location,path_display,state) VALUES(?1,?2,?1,'pending')",
                params![asset, asset.as_bytes()],
            )?;
        }
        while cat.organization_index(100)?.pending {}
        let path = temp.path().join("source.xmp");
        fs::write(&path, br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="3"/></rdf:RDF>"#)?;
        Ok((temp, cat, path))
    }
    #[test]
    fn actual_metadata_calls_obey_priority_fifo_and_cannot_barge() -> Result<()> {
        let (_temp, cat, path) = fixture()?;
        let original = fs::read(&path)?;
        let gate = cat.writers.clone();
        let hold = gate.enter(Priority::Foreground)?;
        let mut workers = Vec::new();
        for (asset, foreground, waiting_fg, waiting_bg) in [
            ("background1", false, 0, 1),
            ("background2", false, 0, 2),
            ("foreground1", true, 1, 2),
            ("foreground2", true, 2, 2),
        ] {
            let root = cat.root.clone();
            let path = path.clone();
            workers.push(thread::spawn(move || -> Result<()> {
                let mut other = Catalog::open(root.join("."))?;
                if foreground {
                    other.organize_asset(asset, 0, Operation::Rating { value: 4 })?;
                } else {
                    let inspection =
                        xmp_packets::inspect_sidecar(&path, &xmp_packets::Limits::default())?;
                    other.retain_metadata(asset, &source(), &inspection)?;
                }
                Ok(())
            }));
            queued(&gate, waiting_fg, waiting_bg);
        }
        // No operation can publish while the preceding transaction's permit exists.
        assert_eq!(
            cat.db
                .query_row("SELECT COUNT(*) FROM metadata_history", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        drop(hold);
        for worker in workers {
            worker.join().expect("writer panicked")?;
        }
        let order = cat
            .db
            .prepare("SELECT asset_id FROM metadata_history ORDER BY id")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(
            order,
            ["foreground1", "foreground2", "background1", "background2"]
        );
        assert_eq!(fs::read(path)?, original);
        for asset in order {
            assert_eq!(cat.render_identity(&asset)?.metadata_revision, 1);
        }
        Ok(())
    }
    #[test]
    fn rollback_unwind_and_recursive_callback_release_authority() -> Result<()> {
        let (_temp, mut cat, path) = fixture()?;
        let inspection = xmp_packets::inspect_sidecar(&path, &xmp_packets::Limits::default())?;
        cat.retain_metadata("background1", &source(), &inspection)?;
        let identity = cat.render_identity("background1")?;
        let other = Catalog::open(&cat.root)?;
        let gate = other.writers.clone();
        let error = cat.with_render_identity(&identity, || -> Result<()> {
            assert!(gate.enter(Priority::Foreground).is_err());
            anyhow::bail!("injected publication failure")
        });
        assert!(error.is_err());
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ =
                cat.with_render_identity(&identity, || -> Result<()> { panic!("injected unwind") });
        }));
        assert!(unwind.is_err());
        let mut other = other;
        assert_eq!(
            other.organize_asset("foreground1", 0, Operation::Rating { value: 5 })?,
            1
        );
        assert!(
            other
                .organize_asset("foreground1", 0, Operation::Rating { value: 2 })
                .is_err()
        );
        assert_eq!(other.render_identity("foreground1")?.metadata_revision, 1);
        let job = other.begin_organization_batch(Operation::Flag {
            value: crate::organization::Flag::Pick,
        })?;
        other.append_organization_batch(
            &job.id,
            &[crate::organization::BatchItem {
                asset_id: "foreground2".into(),
                expected_revision: 0,
            }],
        )?;
        other.seal_organization_batch(&job.id)?;
        let failed = other
            .step_organization_batch_with(&job.id, || anyhow::bail!("rollback written flag"))?;
        assert_eq!(failed.state, "paused");
        assert_eq!(other.render_identity("foreground2")?.metadata_revision, 0);
        assert_eq!(other.db.query_row("SELECT count(*) FROM organization_flags WHERE sequence=(SELECT sequence FROM assets WHERE id='foreground2')", [], |r| r.get::<_,i64>(0))?, 0);
        assert_eq!(
            other.organize_asset("foreground2", 0, Operation::Rating { value: 2 })?,
            1
        );
        let independent = tempfile::tempdir()?;
        let mut independent = Catalog::open(independent.path())?;
        let hold = gate.enter(Priority::Foreground)?;
        independent.create_collection("unrelated", serde_json::Value::Null)?;
        // Current-schema open and read-only snapshot admission do not wait for a writer.
        let opened = Catalog::open(&cat.root)?;
        assert_eq!(opened.render_identity("foreground1")?.metadata_revision, 1);
        drop(hold);
        Ok(())
    }
    #[test]
    fn preview_import_commit_yields_to_foreground_generation_authority() -> Result<()> {
        let (_temp, cat, _path) = fixture()?;
        let gate = cat.writers.clone();
        let held = gate.enter(Priority::Foreground)?;
        let order = Arc::new(Mutex::new(Vec::new()));
        let root = cat.root.clone();
        let background_order = order.clone();
        let background = thread::spawn(move || -> Result<()> {
            let mut other = Catalog::open(root)?;
            let identity = other.render_identity("background1")?;
            let metadata = crate::Metadata {
                format: "JPEG".into(),
                width: 1,
                height: 1,
                orientation: 1,
                camera_make: None,
                camera_model: None,
                captured_at: None,
                lens: None,
                preview_source: "integration fixture".into(),
            };
            assert!(
                other
                    .commit_preview_import(
                        &identity,
                        &"a".repeat(64),
                        &metadata,
                        &"b".repeat(64),
                        || {
                            background_order.lock().unwrap().push("background");
                            Ok(())
                        },
                        || Ok(())
                    )?
                    .is_some()
            );
            Ok(())
        });
        queued(&gate, 0, 1);
        let root = cat.root.clone();
        let foreground_order = order.clone();
        let foreground = thread::spawn(move || -> Result<()> {
            let mut other = Catalog::open(root)?;
            let identity = other.render_identity("foreground1")?;
            assert!(
                other
                    .with_render_identity(&identity, || {
                        foreground_order.lock().unwrap().push("foreground");
                        Ok(())
                    })?
                    .is_some()
            );
            Ok(())
        });
        queued(&gate, 1, 1);
        drop(held);
        foreground.join().expect("foreground panicked")?;
        background.join().expect("background panicked")?;
        assert_eq!(*order.lock().unwrap(), ["foreground", "background"]);
        assert_eq!(cat.render_identity("background1")?.state, "ready");
        Ok(())
    }
}
