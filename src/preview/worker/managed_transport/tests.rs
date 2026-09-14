use super::*;
use crate::application::U64;
use crate::catalog_backup::RestoreStatus;
use crate::catalog_session::{
    CatalogBootstrap, CatalogFilesystem, ConfirmSqlAdmission, LeaseId, PhysicalObjectId,
    PrepareCatalog, RootCapability, SqlAdmissionConfirmed,
};
use crate::preview::{CodecSettings, ServiceLimits, Tier};
fn root() -> RootCapability {
    // Synthetic identities only; these tests do not admit or open a catalog.
    #[cfg(unix)]
    let physical = PhysicalObjectId::Unix {
        device: U64(1),
        inode: U64(2),
    };
    #[cfg(windows)]
    let physical = PhysicalObjectId::Windows {
        volume_serial: U64(1),
        file_index: U64(2),
    };
    #[cfg(unix)]
    let path = std::path::Path::new("/synthetic");
    #[cfg(windows)]
    let path = std::path::Path::new(r"C:\synthetic");
    RootCapability {
        epoch: LeaseId::new(),
        token: LeaseId::new(),
        session: LeaseId::new(),
        canonical_root: NativePath::from_path(path),
        root_physical: physical,
        catalog_physical: physical,
    }
}

struct NoIo;
impl CatalogFilesystem for NoIo {
    fn prepare_catalog(&self, _: &PrepareCatalog, _: &AtomicBool) -> Result<CatalogBootstrap> {
        anyhow::bail!("unexpected catalog admission")
    }
    fn abandon_prepare(&self, _: U64, _: &LeaseId) -> Result<()> {
        anyhow::bail!("unexpected abandon")
    }
    fn confirm_sql_admission(
        &self,
        _: &ConfirmSqlAdmission,
        _: &AtomicBool,
    ) -> Result<SqlAdmissionConfirmed> {
        anyhow::bail!("unexpected SQL admission")
    }
    fn restore_status(&self, _: &RootCapability) -> Result<Option<RestoreStatus>> {
        anyhow::bail!("unexpected restore")
    }
    fn resume_restored_jobs(&self, _: &RootCapability, _: &str, _: bool) -> Result<RestoreStatus> {
        anyhow::bail!("unexpected resume")
    }
    fn release_root(&self, _: &RootCapability) -> Result<()> {
        anyhow::bail!("unexpected root release")
    }
}
#[test]
fn finished_success_is_retained_without_restarting_until_publication_consumes_it() -> Result<()> {
    let key = PreviewKey {
        asset_id: "retained-result".into(),
        variant_id: "master".into(),
        generation: 1,
        image_pixel_generation: Some(1),
        fingerprint: "a".repeat(64),
        edit_revision: 0,
        renderer_version: crate::preview::renderer_identity().into(),
        preparation_version: crate::preview::PREPARATION_VERSION.into(),
        tier: Tier::Thumbnail,
        edge: 1,
        encoding: CodecSettings {
            codec: Codec::Jpeg,
            quality: 80,
        },
    };
    let root = root();
    let work = RenderWork {
        source: root.canonical_root.clone(),
        keys: vec![key.clone()],
        encoded_limit: 1024,
        decode_limits: DecodeLimits::default(),
        edit: None,
    };
    let mut render = Render::new(
        Calls::new(Arc::new(NoIo), root, false),
        work,
        &ServiceLimits::default(),
        1024,
    );
    let pixels = PreparedRgb::new(1, 1, vec![1, 2, 3])?;
    let pointer = pixels.pixels().as_ptr() as usize;
    let batch = RenderedPreviewBatch {
        edit_input: None,
        peak_resident_bytes: None,
        peak_method: "synthetic".into(),
        metadata: Metadata {
            format: "synthetic".into(),
            width: 1,
            height: 1,
            orientation: 1,
            camera_make: None,
            camera_model: None,
            captured_at: None,
            lens: None,
            preview_source: "test".into(),
        },
        provenance: RenderProvenance {
            pipeline_version: "test".into(),
            decoder: "test".into(),
            source_bits_per_channel: 8,
            source_color: "RGB".into(),
            working_color: "RGB".into(),
            alpha: "none".into(),
            calibration: None,
            spatial_calibration: None,
            notes: vec![],
        },
        objects: vec![ProducedPreview {
            key,
            pixels,
            encoded: vec![9, 8],
        }],
        prepared: None,
    };
    render.task = Some(Task::spawn(
        "retained-result-fixture",
        render.cancel.clone(),
        move |_| Ok(Outcome::Work(Ok(Box::new(batch)))),
    )?);
    while render.busy() {
        std::thread::yield_now();
    }
    let canceled = AtomicBool::new(false);
    render.progress(&canceled);
    assert!(render.complete);
    assert!(render.task.is_none());
    for _ in 0..3 {
        render.progress(&canceled);
    }
    assert_eq!(
        render.result.as_ref().unwrap().as_ref().unwrap().objects[0]
            .pixels
            .pixels()
            .as_ptr() as usize,
        pointer
    );
    let batch = render
        .poll(&canceled)?
        .context("retained success missing")?;
    assert_eq!(batch.objects[0].pixels.pixels().as_ptr() as usize, pointer);
    assert_eq!(batch.objects[0].encoded, vec![9, 8]);
    assert!(render.poll(&canceled)?.is_none());
    assert!(render.task.is_none());
    render.stop()?;
    Ok(())
}
