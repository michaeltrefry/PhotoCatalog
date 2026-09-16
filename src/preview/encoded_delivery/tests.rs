use super::*;
use crate::application::U64;
use crate::catalog_session::{
    LeaseId,
    preview_io::{Expected, Object},
};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

struct Held {
    entered: mpsc::SyncSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}
impl crate::preview::AdmittedStoreFiles for Held {
    fn lock_tiers(&self, _: &StoreConfig, _: &str) -> Result<Arc<dyn Send + Sync>> {
        anyhow::bail!("fixture does not acquire locks")
    }
    fn cache_read_cancel(
        &self,
        expected: Expected,
        allowance: u64,
        cancel: &AtomicBool,
    ) -> Result<(Integrity, Vec<u8>)> {
        ensure!(
            expected.bytes.0 == 3 && allowance == 3,
            "exact transfer grant"
        );
        self.entered.send(())?;
        self.release.lock().unwrap().recv()?;
        ensure!(
            cancel.load(Ordering::Acquire),
            "cancellation must reach held I/O"
        );
        Ok((Integrity::Intact, vec![1, 2, 3]))
    }
}
#[test]
fn held_transfer_keeps_exact_encoded_charge_through_join_and_unread_output() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (catalog, mut service, asset, _) = super::super::recovery_tests::setup(directory.path());
    let identity = catalog.edit_render_identity(&VariantKey::master(asset))?;
    let key = service.variant_key(&identity, Tier::Thumbnail)?;
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let io = Arc::new(AtomicBool::new(true));
    let budget = ByteBudget::new(3)?;
    let plan = Plan {
        selected: super::super::super::store::ManagedSelection {
            cached: CachedPreview {
                key: key.clone(),
                bytes: vec![],
                stale: false,
                record: None,
            },
            expected: Expected {
                object: Object {
                    root: LeaseId::new(),
                    key: key.digest()?,
                },
                bytes: U64(3),
                checksum: blake3::hash(&[1, 2, 3]).to_hex().to_string(),
            },
            files: Arc::new(Held {
                entered: entered_tx,
                release: Mutex::new(release_rx),
            }),
        },
        key,
        allowance: 3,
        reservation: budget.try_reserve(3).unwrap(),
        io: IoLease(io.clone()),
    };
    let mut transfer = plan.start(Arc::new(AtomicBool::new(false)))?;
    let entered = entered_rx.recv_timeout(Duration::from_secs(5));
    let before = Instant::now();
    let pending = transfer.poll();
    let elapsed = before.elapsed();
    transfer.signal_cancel();
    let held = (budget.used(), io.load(Ordering::Acquire));
    release_tx.send(())?;
    let output = transfer.shutdown()?.context("retained output")?;
    assert!(entered.is_ok());
    assert!(matches!(pending, Ok(None)));
    assert!(elapsed < Duration::from_secs(1));
    assert_eq!(held, (3, true));
    assert_eq!(budget.used(), 3);
    assert!(io.load(Ordering::Acquire));
    assert_eq!(output.result.as_ref().unwrap().1, [1, 2, 3]);
    drop(output);
    assert_eq!(budget.used(), 0);
    assert!(!io.load(Ordering::Acquire));
    service.try_shutdown()?;
    Ok(())
}
#[test]
fn encoded_delivery_pause_blocks_new_reads_and_restores_admission_on_drop() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (catalog, mut service, asset, _) = super::super::recovery_tests::setup(directory.path());
    let gate = service.pause_for_encoded_delivery()?;
    assert!(
        service
            .ensure_synchronous_read_available()
            .unwrap_err()
            .is::<crate::preview::stage_io::Busy>()
    );
    assert!(service.pause_for_encoded_delivery().is_err());
    let ticket = service.queue_read_variant(
        &catalog,
        &VariantKey::master(asset),
        Tier::Thumbnail,
        false,
        Priority::Foreground,
        false,
    )?;
    assert!(service.tick_read(&catalog).is_none());
    assert!(service.take_read(ticket).is_none());
    assert_eq!(service.encoded.used(), 0);
    drop(gate);
    service.cancel_read(ticket);
    service.ensure_synchronous_read_available()?;
    service.try_shutdown()?;
    Ok(())
}
