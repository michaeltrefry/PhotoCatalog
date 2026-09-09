//! Tiny command-path correctness checks, not performance evidence.
use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::{Value, json};
#[test]
fn actual_probe_commands_validate_identity_artifacts_and_exclusive_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("fixture.png");
    image::RgbImage::from_pixel(8, 4, image::Rgb([128, 128, 128]))
        .save(&source)
        .unwrap();
    let prepared = dir.path().join("prepared");
    cargo_bin_cmd!("preview_probe")
        .arg("prepare")
        .arg(&source)
        .arg(&prepared)
        .assert()
        .success();
    let info: Value =
        serde_json::from_slice(&std::fs::read(prepared.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(info["surfaces"]["256"]["width"], 8);
    assert!(info["identity"]["source_blake3"]["codec"].is_string());
    let mut decoded = None;
    for (codec, quality) in [("jpeg", "65"), ("webp", "65"), ("avif", "60")] {
        let output = dir.path().join(codec);
        cargo_bin_cmd!("preview_probe")
            .args(["measure", "--prepared"])
            .arg(&prepared)
            .args([
                "--edge",
                "256",
                "--codec",
                codec,
                "--quality",
                quality,
                "--output",
            ])
            .arg(&output)
            .assert()
            .success();
        let receipt: Value =
            serde_json::from_slice(&std::fs::read(output.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(receipt["complete"], true);
        assert_eq!(receipt["encode"]["n"], 3);
        assert_eq!(receipt["decode"]["n"], 20);
        assert_eq!(receipt["identity"], info["identity"]);
        assert_eq!(
            receipt["prepared_blake3"],
            info["surfaces"]["256"]["rgb_blake3"]
        );
        decoded = Some(output.join("decoded.png"));
        cargo_bin_cmd!("preview_probe")
            .args(["measure", "--prepared"])
            .arg(&prepared)
            .args([
                "--edge",
                "256",
                "--codec",
                codec,
                "--quality",
                quality,
                "--output",
            ])
            .arg(&output)
            .assert()
            .failure();
    }
    let review = dir.path().join("review");
    let manifest = dir.path().join("review.json");
    std::fs::write(&manifest,serde_json::to_vec(&json!({"reference":prepared.join("256.png"),"candidates":(0..9).map(|i|json!({"label":char::from(b'A'+i).to_string(),"path":decoded})).collect::<Vec<_>>()})).unwrap()).unwrap();
    cargo_bin_cmd!("preview_probe")
        .arg("review")
        .arg(manifest)
        .arg(&review)
        .assert()
        .success();
    assert!(review.join("contact.png").exists());
    assert!(review.join("I-crop-4.png").exists());
}
