use photocatalog::preview::{
    Codec, CodecSettings, PREPARATION_VERSION, PreviewKey, RenderWork, Tier, WorkerProcess,
    renderer_identity,
};
use photocatalog::storage_volume::NativePath;
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

fn work(root: &Path) -> RenderWork {
    let path = root.join("original.png");
    image::RgbImage::from_fn(17, 11, |x, y| {
        image::Rgb([(x * 13) as u8, (y * 19) as u8, ((x + y) * 7) as u8])
    })
    .save(&path)
    .unwrap();
    let fingerprint = blake3::hash(&std::fs::read(&path).unwrap())
        .to_hex()
        .to_string();
    let keys = [(Tier::Thumbnail, 512), (Tier::Large, 1600)]
        .into_iter()
        .map(|(tier, edge)| PreviewKey {
            asset_id: "worker-test".into(),
            variant_id: "master".into(),
            generation: 1,
            fingerprint: fingerprint.clone(),
            edit_revision: 0,
            renderer_version: renderer_identity().into(),
            preparation_version: PREPARATION_VERSION.into(),
            tier,
            edge,
            encoding: CodecSettings {
                codec: Codec::Jpeg,
                quality: 80,
            },
        })
        .collect();
    RenderWork {
        source: NativePath::from_path(&path),
        keys,
        encoded_limit: 1024 * 1024,
        decode_limits: photocatalog::media::DecodeLimits::default(),
    }
}
fn native_work(root: &Path) -> RenderWork {
    let mut request = work(root);
    let path = root.join("original.dng");
    std::fs::write(&path, include_bytes!("fixtures/generated-linear-mask.dng")).unwrap();
    let fingerprint = blake3::hash(&std::fs::read(&path).unwrap())
        .to_hex()
        .to_string();
    request.source = NativePath::from_path(&path);
    for key in &mut request.keys {
        key.fingerprint = fingerprint.clone();
    }
    request
}
fn executable() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_photocatalog"))
}
#[test]
fn actual_worker_renders_both_tiers_and_validates_full_outputs() {
    let root = tempfile::tempdir().unwrap();
    let request = work(root.path());
    let source = request.source.to_path().unwrap();
    let before = std::fs::read(&source).unwrap();
    let mut worker =
        WorkerProcess::spawn(executable(), &root.path().join("staging"), request.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let result = loop {
        if let Some(result) = worker.poll(&AtomicBool::new(false)).unwrap() {
            break result;
        }
        assert!(Instant::now() < deadline, "worker did not complete");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(result.objects.len(), 2);
    for (object, key) in result.objects.iter().zip(&request.keys) {
        assert_eq!(&object.key, key);
        assert_eq!((object.pixels.width(), object.pixels.height()), (17, 11));
        assert!(object.encoded.ends_with(&[0xff, 0xd9]));
    }
    assert_eq!(result.provenance.pipeline_version, "photocatalog-render-4");
    assert_eq!(std::fs::read(source).unwrap(), before);
    drop(worker);
    assert_eq!(
        std::fs::read_dir(root.path().join("staging"))
            .unwrap()
            .count(),
        0
    );
}
#[test]
fn actual_child_cancellation_waits_and_cleans_private_staging() {
    let root = tempfile::tempdir().unwrap();
    let request = native_work(root.path());
    let source = request.source.to_path().unwrap();
    let original_hash = request.keys[0].fingerprint.clone();
    let mut worker =
        WorkerProcess::spawn(executable(), &root.path().join("staging"), request).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !worker.awaiting_encode_admission().unwrap() {
        assert!(
            Instant::now() < deadline,
            "worker did not reach decoded holding checkpoint"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        worker
            .poll(&AtomicBool::new(true))
            .err()
            .unwrap()
            .to_string()
            .contains("canceled")
    );
    drop(worker);
    assert_eq!(
        blake3::hash(&std::fs::read(source).unwrap())
            .to_hex()
            .as_str(),
        original_hash
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("staging"))
            .unwrap()
            .count(),
        0
    );
}
#[test]
fn owner_pipe_eof_terminates_child_without_a_graceful_cancel_command() {
    owner_eof(false);
}
#[test]
fn owner_pipe_eof_terminates_actual_post_decode_holding_process() {
    owner_eof(true);
}
fn owner_eof(after_decode: bool) {
    let root = tempfile::tempdir().unwrap();
    let request = if after_decode {
        native_work(root.path())
    } else {
        work(root.path())
    };
    let stage = tempfile::Builder::new()
        .prefix("worker-")
        .tempdir_in(root.path())
        .unwrap();
    let mut child = Command::new(executable())
        .arg("--preview-worker")
        .current_dir(stage.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    input
        .write_all(if after_decode { b"\n!" } else { b"\n" })
        .unwrap();
    input.flush().unwrap();
    if after_decode {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !stage.path().join("decoded.ready").exists() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("decoded checkpoint not reached");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            std::fs::read(stage.path().join("decoded.ready")).unwrap(),
            b"decoded"
        );
    }
    // Kernel pipe EOF is also what the child observes when its owner crashes.
    // The post-decode case exercises the armed watchdog with decoded pixels held.
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(!status.success());
            if after_decode {
                assert_eq!(status.code(), Some(74));
            }
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("orphan worker survived EOF");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!stage.path().join("0.preview").exists());
    assert_eq!(
        blake3::hash(&std::fs::read(request.source.to_path().unwrap()).unwrap())
            .to_hex()
            .as_str(),
        request.keys[0].fingerprint
    );
}

#[test]
fn actual_worker_preserves_resource_refusal_for_retry_with_more_allowance() {
    use photocatalog::{media::DecodeStatus, preview::WorkerFailure};
    let root = tempfile::tempdir().unwrap();
    let mut request = native_work(root.path());
    request.decode_limits.max_intermediate_pixels = 1;
    let mut worker =
        WorkerProcess::spawn(executable(), &root.path().join("staging"), request.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let error = loop {
        match worker.poll(&AtomicBool::new(false)) {
            Err(error) => break error,
            Ok(None) => {}
            Ok(Some(_)) => panic!("limited worker unexpectedly completed"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(
        error.downcast_ref::<WorkerFailure>().unwrap().decode_status,
        Some(DecodeStatus::ResourceLimit)
    );
    assert!(!worker.awaiting_encode_admission().unwrap());
    drop(worker);
    request.decode_limits = photocatalog::media::DecodeLimits::default();
    let mut worker =
        WorkerProcess::spawn(executable(), &root.path().join("staging"), request).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if worker.poll(&AtomicBool::new(false)).unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}
