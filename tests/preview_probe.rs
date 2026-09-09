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
    let mut decoded_digest = Value::Null;
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
        cargo_bin_cmd!("preview_probe")
            .arg("verify-case")
            .arg(&prepared)
            .arg(&output)
            .assert()
            .success();
        // Artifact admission must reject same-sized invalid data, forged digests,
        // missing PNGs and a valid PNG with different pixels.
        let receipt_path = output.join("receipt.json");
        let original_receipt = std::fs::read(&receipt_path).unwrap();
        let encoded_path = output.join(format!(
            "encode-0.{}",
            if codec == "jpeg" { "jpg" } else { codec }
        ));
        let original_encoded = std::fs::read(&encoded_path).unwrap();
        std::fs::write(&encoded_path, vec![0; original_encoded.len()]).unwrap();
        cargo_bin_cmd!("preview_probe")
            .arg("verify-case")
            .arg(&prepared)
            .arg(&output)
            .assert()
            .failure();
        std::fs::write(&encoded_path, &original_encoded).unwrap();
        let mut forged = receipt.clone();
        forged["artifacts"][0]["blake3"] = json!("0".repeat(64));
        std::fs::write(&receipt_path, serde_json::to_vec(&forged).unwrap()).unwrap();
        cargo_bin_cmd!("preview_probe")
            .arg("verify-case")
            .arg(&prepared)
            .arg(&output)
            .assert()
            .failure();
        std::fs::write(&receipt_path, &original_receipt).unwrap();
        let png = output.join("decoded.png");
        let original_png = std::fs::read(&png).unwrap();
        std::fs::remove_file(&png).unwrap();
        cargo_bin_cmd!("preview_probe")
            .arg("verify-case")
            .arg(&prepared)
            .arg(&output)
            .assert()
            .failure();
        image::RgbImage::from_pixel(8, 4, image::Rgb([0, 0, 0]))
            .save(&png)
            .unwrap();
        cargo_bin_cmd!("preview_probe")
            .arg("verify-case")
            .arg(&prepared)
            .arg(&output)
            .assert()
            .failure();
        std::fs::write(&png, &original_png).unwrap();
        decoded_digest = receipt["decoded_blake3"].clone();
        decoded = Some(png);
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
    std::fs::write(&manifest,serde_json::to_vec(&json!({"reference":prepared.join("256.png"),"reference_blake3":info["surfaces"]["256"]["rgb_blake3"],"candidates":(0..9).map(|i|json!({"label":char::from(b'A'+i).to_string(),"path":decoded,"decoded_blake3":decoded_digest})).collect::<Vec<_>>()})).unwrap()).unwrap();
    cargo_bin_cmd!("preview_probe")
        .arg("review")
        .arg(&manifest)
        .arg(&review)
        .assert()
        .success();
    assert!(review.join("contact.png").exists());
    assert!(review.join("I-crop-4.png").exists());
    cargo_bin_cmd!("preview_probe")
        .arg("verify-review")
        .arg(&review)
        .assert()
        .success();
    std::fs::write(review.join("A.png"), b"corrupt").unwrap();
    cargo_bin_cmd!("preview_probe")
        .arg("verify-review")
        .arg(&review)
        .assert()
        .failure();
    let mut bad: Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    bad["candidates"][0]["decoded_blake3"] = json!("0".repeat(64));
    std::fs::write(&manifest, serde_json::to_vec(&bad).unwrap()).unwrap();
    cargo_bin_cmd!("preview_probe")
        .arg("review")
        .arg(&manifest)
        .arg(dir.path().join("bad-review"))
        .assert()
        .failure();
}
