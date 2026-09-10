use photocatalog::{
    edit::{
        OriginalRequest, Recipe, RenderError, RenderLimits, RenderPurpose, WhiteBalance,
        decode_original, render_recipe,
    },
    media::{DecodeLimits, decode_full},
};
#[test]
fn original_dng_wb_masks_fingerprint_and_decode_admission() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.dng");
    let source = include_bytes!("fixtures/generated-linear-mask.dng");
    std::fs::write(&path, source).unwrap();
    let hash = blake3::hash(source).to_hex().to_string();
    let wb = WhiteBalance::AsShot;
    let request = || OriginalRequest {
        path: &path,
        expected_fingerprint: &hash,
        white_balance: &wb,
    };
    assert!(matches!(
        decode_original(
            request(),
            DecodeLimits {
                max_allocation_bytes: 16,
                ..Default::default()
            },
            &()
        ),
        Err(RenderError::Decode(_))
    ));
    let input = decode_original(
        request(),
        DecodeLimits {
            max_allocation_bytes: 2 * 1024 * 1024,
            ..Default::default()
        },
        &(),
    )
    .unwrap();
    let baseline = decode_full(&path).unwrap();
    assert_eq!(input.pixels(), baseline.pixels);
    let edited = render_recipe(
        &input,
        &Recipe::default().validate().unwrap(),
        RenderPurpose::ExportExact,
        RenderLimits::default(),
        &(),
    )
    .unwrap();
    assert_eq!(edited.as_rendered().pixels, baseline.pixels);
    let custom = WhiteBalance::TemperatureTint {
        kelvin: 3200,
        tint: 10.,
    };
    let custom = decode_original(
        OriginalRequest {
            path: &path,
            expected_fingerprint: &hash,
            white_balance: &custom,
        },
        DecodeLimits::default(),
        &(),
    )
    .unwrap();
    assert!(
        custom
            .pixels()
            .iter()
            .zip(&baseline.pixels)
            .any(|(a, b)| (a[0] - b[0]).abs() > 0.01)
    );
    for (a, b) in custom.pixels().iter().zip(&baseline.pixels) {
        assert_eq!(a[3], b[3]);
        assert!(a.iter().all(|v| v.is_finite()));
    }
    assert_eq!(
        blake3::hash(&std::fs::read(&path).unwrap())
            .to_hex()
            .to_string(),
        hash
    );
    std::fs::write(&path, b"changed source").unwrap();
    assert!(matches!(
        decode_original(request(), DecodeLimits::default(), &()),
        Err(RenderError::SourceChanged)
    ));
}
