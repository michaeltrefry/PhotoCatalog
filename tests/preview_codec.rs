use photocatalog::{
    media::{Metadata, RenderProvenance, RenderedImage},
    preview::{Codec, CodecSettings, PreparedRgb, decode, encode, prepare},
};
use std::sync::atomic::AtomicBool;
fn rendered(w: u32, h: u32, pixels: Vec<[f32; 4]>) -> RenderedImage {
    RenderedImage {
        width: w,
        height: h,
        pixels,
        metadata: Metadata {
            format: "test".into(),
            width: w,
            height: h,
            orientation: 1,
            camera_make: None,
            camera_model: None,
            captured_at: None,
            lens: None,
            preview_source: "independent fixture".into(),
        },
        provenance: RenderProvenance {
            pipeline_version: "test".into(),
            decoder: "test".into(),
            source_bits_per_channel: 32,
            source_color: "linear sRGB".into(),
            working_color: "linear sRGB".into(),
            alpha: "straight".into(),
            calibration: None,
            spatial_calibration: None,
            notes: vec![],
        },
    }
}
fn oracle(v: f32, a: f32) -> u8 {
    let x = f64::from(v) * f64::from(a) + 1.0 - f64::from(a);
    let y = if x <= 0.0031308 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    };
    (y.clamp(0.0, 1.0) * 255.0).round() as u8
}
#[test]
fn transfer_and_alpha_match_independent_float64_oracle() {
    let samples = [-0.1, 0.0, 0.0031308, 0.18, 0.5, 1.0, 2.0];
    for alpha in [0.0, 0.5, 1.0] {
        let input = rendered(7, 1, samples.map(|x| [x, x, x, alpha]).to_vec());
        let output = prepare(&input, 512).unwrap();
        assert_eq!((output.width(), output.height()), (7, 1)); // no upscaling
        assert_eq!(
            output.pixels(),
            samples
                .into_iter()
                .flat_map(|x| [oracle(x, alpha); 3])
                .collect::<Vec<_>>()
        );
    }
}
#[test]
fn quantization_precedes_block_resize() {
    let input = rendered(
        4,
        2,
        vec![
            [0., 0., 0., 1.],
            [1., 1., 1., 1.],
            [0.18, 0.18, 0.18, 1.],
            [0.5, 0.5, 0.5, 1.],
            [0., 0., 0., 1.],
            [1., 1., 1., 1.],
            [0.18, 0.18, 0.18, 1.],
            [0.5, 0.5, 0.5, 1.],
        ],
    );
    let actual = prepare(&input, 2).unwrap();
    // The pinned integer thumbnail averages quantized channels and rounds ties up.
    let expected = [
        128,
        (u16::from(oracle(0.18, 1.)) + u16::from(oracle(0.5, 1.))).div_ceil(2) as u8,
    ];
    assert_eq!((actual.width(), actual.height()), (2, 1));
    assert_eq!(
        actual.pixels(),
        expected
            .into_iter()
            .flat_map(|x| [x; 3])
            .collect::<Vec<_>>()
    );
}
#[test]
fn orientation_is_applied_once_before_preparation() {
    use image::ImageEncoder;
    // Source RGB labels are independently mapped for each EXIF transform.
    let colors = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 0],
        [255, 0, 255],
        [0, 255, 255],
    ];
    let orders: [([usize; 6], u32, u32); 8] = [
        ([0, 1, 2, 3, 4, 5], 2, 3),
        ([1, 0, 3, 2, 5, 4], 2, 3),
        ([5, 4, 3, 2, 1, 0], 2, 3),
        ([4, 5, 2, 3, 0, 1], 2, 3),
        ([0, 2, 4, 1, 3, 5], 3, 2),
        ([4, 2, 0, 5, 3, 1], 3, 2),
        ([5, 3, 1, 4, 2, 0], 3, 2),
        ([1, 3, 5, 0, 2, 4], 3, 2),
    ];
    let dir = tempfile::tempdir().unwrap();
    for (index, (order, w, h)) in orders.into_iter().enumerate() {
        let mut exif = b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\0\0\0\0\0\0\0\0".to_vec();
        exif[18] = (index + 1) as u8;
        let path = dir.path().join(format!("orientation-{index}.png"));
        let mut data = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut data);
        encoder.set_exif_metadata(exif).unwrap();
        encoder
            .write_image(&colors.concat(), 2, 3, image::ExtendedColorType::Rgb8)
            .unwrap();
        std::fs::write(&path, data).unwrap();
        let full = photocatalog::media::decode_full(&path).unwrap();
        let actual = prepare(&full, 512).unwrap();
        assert_eq!((actual.width(), actual.height()), (w, h));
        assert_eq!(
            actual.pixels(),
            order
                .into_iter()
                .flat_map(|i| colors[i])
                .collect::<Vec<_>>()
        );
    }
}
#[test]
fn codec_roundtrips_materialize_rgb8_and_reject_bad_input() {
    let rgb = PreparedRgb::new(64, 32, vec![128; 64 * 32 * 3]).unwrap();
    for codec in [Codec::Jpeg, Codec::Webp, Codec::Avif] {
        let bytes = encode(&rgb, CodecSettings { codec, quality: 65 }, None).unwrap();
        let result = decode(&bytes, codec).unwrap();
        assert_eq!((result.width(), result.height()), (64, 32));
        assert!(result.pixels().iter().all(|v| v.abs_diff(128) <= 3));
        assert!(decode(&bytes[..bytes.len() / 2], codec).is_err());
        assert!(
            encode(
                &rgb,
                CodecSettings { codec, quality: 65 },
                Some(&AtomicBool::new(true))
            )
            .is_err()
        );
    }
}
#[test]
fn invalid_prepared_surfaces_fail_before_native_work() {
    assert!(PreparedRgb::new(0, 1, vec![]).is_err());
    assert!(PreparedRgb::new(8193, 1, vec![]).is_err());
    assert!(PreparedRgb::new(1, 1, vec![0, 1]).is_err());
    assert!(prepare(&rendered(1, 1, vec![[f32::NAN, 0., 0., 1.]]), 1).is_err());
    assert!(prepare(&rendered(1, 1, vec![[0., 0., 0., 2.]]), 1).is_err());
}
