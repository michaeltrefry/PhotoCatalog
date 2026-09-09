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
    }
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
    let request = work(root.path());
    let mut worker =
        WorkerProcess::spawn(executable(), &root.path().join("staging"), request).unwrap();
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
        std::fs::read_dir(root.path().join("staging"))
            .unwrap()
            .count(),
        0
    );
}
#[test]
fn owner_pipe_eof_terminates_child_without_a_graceful_cancel_command() {
    let root = tempfile::tempdir().unwrap();
    let request = work(root.path());
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
    input.write_all(b"\n").unwrap();
    input.flush().unwrap();
    // Kernel pipe EOF is also what the child observes when its owner crashes.
    // No start token: deterministic proof that orphan work cannot start later.
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(!status.success());
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
}
