//! Tiny complete probe/worker/verifier contract; no private corpus or campaign.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn memory_probe_retains_complete_outputs_and_rejects_changed_saved_bytes() {
    use std::{fs, time::Duration};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.dng");
    let original = include_bytes!("fixtures/generated-linear-mask.dng");
    fs::write(&source, original).unwrap();
    let limits = root.path().join("limits.json");
    fs::write(
        &limits,
        serde_json::to_vec(&photocatalog::media::DecodeLimits::default()).unwrap(),
    )
    .unwrap();
    let output = root.path().join("worker-result");
    let probe = assert_cmd::cargo::cargo_bin!("preview_runtime_probe");
    assert_cmd::Command::new(probe)
        .timeout(Duration::from_secs(15))
        .arg("worker")
        .arg("--worker")
        .arg(assert_cmd::cargo::cargo_bin!("photocatalog"))
        .arg("--source")
        .arg(&source)
        .arg("--fixture-id")
        .arg("generated-linear-mask")
        .arg("--limits")
        .arg(limits)
        .arg("--output")
        .arg(&output)
        .assert()
        .success();
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["complete"], true);
    assert!(receipt["worker_peak_rss_bytes"].as_u64().unwrap() > 0);
    assert_eq!(receipt["artifacts"].as_array().unwrap().len(), 2);
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_cmd::Command::new(probe)
        .timeout(Duration::from_secs(15))
        .arg("verify")
        .arg(&output)
        .assert()
        .success();
    let path = output.join("512.jpg");
    let mut changed = fs::read(&path).unwrap();
    let index = changed.len() / 2;
    changed[index] ^= 1;
    fs::write(path, changed).unwrap();
    assert_cmd::Command::new(probe)
        .timeout(Duration::from_secs(15))
        .arg("verify")
        .arg(&output)
        .assert()
        .failure();
    assert_eq!(fs::read(source).unwrap(), original);
}
