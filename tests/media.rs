use image::{ImageEncoder, ImageFormat, Rgba, RgbaImage};
use photocatalog::media::{DecodeStatus, decode_full};
use std::io::Cursor;

#[test]
fn raster_formats_decode_originals_and_keep_straight_alpha() {
    let dir = tempfile::tempdir().unwrap();
    let pixels = RgbaImage::from_fn(3, 2, |x, y| {
        Rgba([
            (x * 71) as u8,
            (y * 131) as u8,
            255,
            if x == 1 { 128 } else { 255 },
        ])
    });
    for (name, format) in [
        ("sample.png", ImageFormat::Png),
        ("sample.webp", ImageFormat::WebP),
        ("sample.bmp", ImageFormat::Bmp),
        ("sample.tiff", ImageFormat::Tiff),
    ] {
        let path = dir.path().join(name);
        pixels.save_with_format(&path, format).unwrap();
        let image = decode_full(&path).unwrap();
        assert_eq!((image.width, image.height), (3, 2));
        assert!((image.pixels[1][3] - 128.0 / 255.0).abs() < 1e-6);
        assert!((image.pixels[1][2] - 1.0).abs() < 1e-6);
        assert_eq!(image.provenance.source_bits_per_channel, 8);
        assert_eq!(std::fs::read(&path).unwrap(), {
            let mut out = Cursor::new(Vec::new());
            pixels.write_to(&mut out, format).unwrap();
            out.into_inner()
        });
    }
}

#[test]
fn high_bit_depth_is_not_reduced_to_eight_bits() {
    let dir = tempfile::tempdir().unwrap();
    let image = image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::from_raw(
        2,
        1,
        vec![30000, 30001, 30002, 32768, 30003, 30004, 30005, 65535],
    )
    .unwrap();
    for (name, format) in [
        ("precision.png", ImageFormat::Png),
        ("precision.tiff", ImageFormat::Tiff),
    ] {
        let path = dir.path().join(name);
        image.save_with_format(&path, format).unwrap();
        let rendered = decode_full(&path).unwrap();
        assert_eq!(rendered.provenance.source_bits_per_channel, 16);
        assert!(
            rendered.pixels[0][0] < rendered.pixels[0][1]
                && rendered.pixels[0][1] < rendered.pixels[0][2]
        );
        assert!((rendered.pixels[0][3] - 32768.0 / 65535.0).abs() < 1e-6);
    }
}

#[test]
fn embedded_linear_icc_is_applied_and_alpha_is_unmodified() {
    let dir = tempfile::tempdir().unwrap();
    let curve = lcms2::ToneCurve::new(1.0);
    let point = |x, y| lcms2::CIExyY { x, y, Y: 1.0 };
    let profile = lcms2::Profile::new_rgb(
        &point(0.3127, 0.3290),
        &lcms2::CIExyYTRIPLE {
            Red: point(0.64, 0.33),
            Green: point(0.30, 0.60),
            Blue: point(0.15, 0.06),
        },
        &[&curve, &curve, &curve],
    )
    .unwrap();
    let mut bytes = Vec::new();
    let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
    encoder.set_icc_profile(profile.icc().unwrap()).unwrap();
    encoder
        .write_image(&[128, 128, 128, 64], 1, 1, image::ExtendedColorType::Rgba8)
        .unwrap();
    let path = dir.path().join("linear.png");
    std::fs::write(&path, bytes).unwrap();
    let rendered = decode_full(&path).unwrap();
    for c in 0..3 {
        assert!(
            (rendered.pixels[0][c] - 128.0 / 255.0).abs() < 0.0001,
            "{:?}",
            rendered.pixels[0]
        );
    }
    assert!((rendered.pixels[0][3] - 64.0 / 255.0).abs() < 1e-6);
}

#[test]
fn corrupt_and_unsupported_are_observable() {
    let dir = tempfile::tempdir().unwrap();
    let unknown = dir.path().join("unknown.xyz");
    std::fs::write(&unknown, b"not an image").unwrap();
    assert_eq!(
        decode_full(&unknown).err().unwrap().status,
        DecodeStatus::Unsupported
    );
    let corrupt = dir.path().join("broken.png");
    std::fs::write(&corrupt, b"\x89PNG\r\n\x1a\n").unwrap();
    assert_eq!(
        decode_full(&corrupt).err().unwrap().status,
        DecodeStatus::Corrupt
    );
    let missing = dir.path().join("missing.jpg");
    assert_eq!(
        decode_full(&missing).err().unwrap().status,
        DecodeStatus::Io
    );
}

#[test]
fn psd_rgb_composite_precision_and_corrupt_bounds() {
    let mut bytes = b"8BPS\0\x01\0\0\0\0\0\0".to_vec();
    bytes.extend(3u16.to_be_bytes());
    bytes.extend(1u32.to_be_bytes());
    bytes.extend(2u32.to_be_bytes());
    bytes.extend(16u16.to_be_bytes());
    bytes.extend(3u16.to_be_bytes());
    for _ in 0..3 {
        bytes.extend(0u32.to_be_bytes());
    }
    bytes.extend(0u16.to_be_bytes());
    for value in [30000u16, 30001, 30002, 30003, 30004, 30005] {
        bytes.extend(value.to_be_bytes());
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("composite.psd");
    std::fs::write(&path, &bytes).unwrap();
    let refusal = photocatalog::media::decode_full_limited(
        &path,
        photocatalog::media::DecodeLimits {
            max_intermediate_pixels: 1,
            ..Default::default()
        },
    );
    assert_eq!(refusal.err().unwrap().status, DecodeStatus::ResourceLimit);

    let rendered = decode_full(&path).unwrap();
    assert_eq!(rendered.provenance.source_bits_per_channel, 16);
    assert_eq!((rendered.width, rendered.height), (2, 1));
    assert!(rendered.pixels[0][0] < rendered.pixels[1][0]);
    bytes.pop();
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        decode_full(&path).err().unwrap().status,
        DecodeStatus::Corrupt
    );
}

#[test]
fn linear_dng_profile_matrix_matches_independent_oracle_and_retains_mask() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("linear.dng");
    std::fs::write(&path, include_bytes!("fixtures/generated-linear-mask.dng")).unwrap();
    let image = decode_full(&path).unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/generated-linear-mask.expected.json")).unwrap();
    assert_eq!((image.width, image.height), (36, 16));
    for x in 0..6 {
        for c in 0..3 {
            let target = expected["linear_srgb"][x][c].as_f64().unwrap() as f32;
            assert!(
                (image.pixels[x][c] - target).abs() < 0.001,
                "pixel{x} channel{c}: {} expected {target}",
                image.pixels[x][c]
            );
        }
        assert!((image.pixels[x][3] - expected["alpha"][x].as_f64().unwrap() as f32).abs() < 1e-6);
    }
    assert!(image.pixels[5][0] > 1.0);
    assert!(image.pixels[5][1] < 0.0);
    assert_eq!(image.pixels, decode_full(&path).unwrap().pixels);
}

#[test]
fn embedded_dng_calibration_matches_analytic_lut_and_bypasses_undefined_samples() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("calibration.dng");
    std::fs::write(
        &path,
        include_bytes!("fixtures/generated-calibration-mask.dng"),
    )
    .unwrap();
    let image = decode_full(&path).unwrap();
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/generated-calibration-mask.expected.json"
    ))
    .unwrap();
    assert_eq!((image.width, image.height), (36, 16));
    for x in 0..6 {
        for c in 0..3 {
            let target = expected["linear_srgb"][x][c].as_f64().unwrap() as f32;
            assert!(
                (image.pixels[x][c] - target).abs() < 0.001,
                "calibration pixel{x} channel{c}: {} expected {target}",
                image.pixels[x][c]
            );
        }
        assert!((image.pixels[x][3] - expected["alpha"][x].as_f64().unwrap() as f32).abs() < 1e-6);
    }
    let calibration = image.provenance.calibration.as_ref().unwrap();
    let applied = expected["calibration_applied"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| x.as_bool() == Some(true))
        .count() as u64
        * 16
        * 6;
    assert_eq!(calibration.applied_pixels, applied);
    assert_eq!(calibration.bypassed_pixels, 36 * 16 - applied);
    assert!(calibration.applied_pixels > 0 && calibration.bypassed_pixels > 0);
    assert!(image.pixels[5][0] > 1.0);
    assert!(image.pixels[3][0] < 0.0);
    assert_eq!(image.pixels, decode_full(&path).unwrap().pixels);
}

#[test]
fn avif_ten_bit_color_and_alpha_match_lossless_swatches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swatches.avif");
    std::fs::write(
        &path,
        include_bytes!("fixtures/generated-swatches-10bit.avif"),
    )
    .unwrap();
    let image = decode_full(&path).unwrap();
    assert_eq!((image.width, image.height), (3, 2));
    assert_eq!(image.provenance.source_bits_per_channel, 10);
    for (index, expected) in [
        (0, [1.0, 0.0, 0.0, 1.0]),
        (1, [0.0, 1.0, 0.0, 128.0 / 255.0]),
        (4, [1.0, 1.0, 1.0, 1.0]),
    ] {
        for (actual, target) in image.pixels[index].iter().zip(expected) {
            assert!((actual - target).abs() < 0.003, "{:?}", image.pixels[index]);
        }
    }
}

#[test]
fn all_exif_orientations_are_applied_once_at_full_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let orders = [
        vec![1, 2, 3, 4, 5, 6],
        vec![3, 2, 1, 6, 5, 4],
        vec![6, 5, 4, 3, 2, 1],
        vec![4, 5, 6, 1, 2, 3],
        vec![1, 4, 2, 5, 3, 6],
        vec![4, 1, 5, 2, 6, 3],
        vec![6, 3, 5, 2, 4, 1],
        vec![3, 6, 2, 5, 1, 4],
    ];
    for orientation in 1u16..=8 {
        let mut exif = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
        exif.extend(orientation.to_le_bytes());
        exif.extend([0; 6]);
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        encoder.set_exif_metadata(exif).unwrap();
        let samples: Vec<u8> = (1..=6).flat_map(|n| [n * 30, 0, 0]).collect();
        encoder
            .write_image(&samples, 3, 2, image::ExtendedColorType::Rgb8)
            .unwrap();
        let path = dir.path().join("orientation.png");
        std::fs::write(&path, bytes).unwrap();
        let image = decode_full(&path).unwrap();
        assert_eq!(
            (image.width, image.height),
            if orientation >= 5 { (2, 3) } else { (3, 2) }
        );
        for (pixel, n) in image.pixels.iter().zip(&orders[orientation as usize - 1]) {
            let v = (*n as f32 * 30.0 / 255.0 + 0.055) / 1.055;
            assert!((pixel[0] - v.powf(2.4)).abs() < 1e-6);
        }
    }
}

#[test]
fn psd_merged_transparency_tags_and_missing_composite_are_observable() {
    let dir = tempfile::tempdir().unwrap();
    for (depth, key) in [(8u16, b"Mtrn"), (16, b"Mt16"), (32, b"Mt32")] {
        let mut bytes = b"8BPS\0\x01\0\0\0\0\0\0".to_vec();
        bytes.extend(4u16.to_be_bytes());
        bytes.extend(1u32.to_be_bytes());
        bytes.extend(2u32.to_be_bytes());
        bytes.extend(depth.to_be_bytes());
        bytes.extend(3u16.to_be_bytes());
        bytes.extend(0u32.to_be_bytes()); // color mode
        bytes.extend(0u32.to_be_bytes()); // resources
        bytes.extend(22u32.to_be_bytes()); // layer and mask section with zero padding
        bytes.extend(0u32.to_be_bytes()); // layer info
        bytes.extend(0u32.to_be_bytes()); // global mask
        bytes.extend(b"8BIM");
        bytes.extend(key);
        bytes.extend(0u32.to_be_bytes());
        bytes.extend([0, 0]); // trailing layer/mask section padding
        bytes.extend(0u16.to_be_bytes());
        for value in [1.0f32, 1.0, 1.0, 0.75, 1.0, 0.75, 0.0, 0.5] {
            match depth {
                8 => bytes.push((value * 255.0).round() as u8),
                16 => bytes.extend(((value * 65535.0).round() as u16).to_be_bytes()),
                _ => bytes.extend(value.to_be_bytes()),
            }
        }
        let path = dir.path().join(format!("alpha-{depth}.psd"));
        std::fs::write(&path, bytes).unwrap();
        let image = decode_full(&path).unwrap();
        assert_eq!(image.pixels[0][3], 0.0);
        assert!((image.pixels[1][3] - 0.5).abs() < 0.002);
        // Stored green/blue .75 with half alpha unmattes to .5 before sRGB decoding.
        assert!((image.pixels[1][1] - 0.214041).abs() < 0.004);
        assert!((image.pixels[1][2] - 0.214041).abs() < 0.004);
    }
    let mut bytes = b"8BPS\0\x01\0\0\0\0\0\0".to_vec();
    bytes.extend(3u16.to_be_bytes());
    bytes.extend(1u32.to_be_bytes());
    bytes.extend(1u32.to_be_bytes());
    bytes.extend(8u16.to_be_bytes());
    bytes.extend(3u16.to_be_bytes());
    bytes.extend(0u32.to_be_bytes());
    let mut resource = b"8BIM".to_vec();
    resource.extend(1057u16.to_be_bytes());
    resource.extend([0, 0]);
    resource.extend(17u32.to_be_bytes());
    resource.extend(1u32.to_be_bytes());
    resource.push(0);
    resource.extend(0u32.to_be_bytes());
    resource.extend(0u32.to_be_bytes());
    resource.extend(1u32.to_be_bytes());
    resource.push(0);
    bytes.extend((resource.len() as u32).to_be_bytes());
    bytes.extend(resource);
    bytes.extend(0u32.to_be_bytes());
    bytes.extend(0u16.to_be_bytes());
    bytes.extend([255, 0, 0]);
    let path = dir.path().join("no-merged.psd");
    std::fs::write(&path, bytes).unwrap();
    let failure = decode_full(&path).err().unwrap();
    assert_eq!(failure.status, DecodeStatus::Unsupported);
    assert!(failure.message.contains("real merged"));
}

#[test]
fn avif_container_orientation_takes_precedence_over_exif() {
    let dir = tempfile::tempdir().unwrap();
    let mut rendered = Vec::new();
    for (name, bytes) in [
        (
            "irot.avif",
            include_bytes!("fixtures/generated-irot.avif").as_slice(),
        ),
        (
            "irot-exif.avif",
            include_bytes!("fixtures/generated-irot-exif.avif").as_slice(),
        ),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        let image = decode_full(&path).unwrap();
        assert_eq!((image.width, image.height), (2, 3));
        assert_eq!(image.metadata.orientation, 8);
        rendered.push(image.pixels);
    }
    assert_eq!(rendered[0], rendered[1]);
}

#[test]
fn dng_spatial_gain_matches_independent_position_intensity_gamma_oracle() {
    let dir = tempfile::tempdir().unwrap();
    for (index, bytes, oracle) in [
        (
            1,
            include_bytes!("fixtures/generated-spatial1-mask.dng").as_slice(),
            include_str!("fixtures/generated-spatial1-mask.expected.json"),
        ),
        (
            2,
            include_bytes!("fixtures/generated-spatial2-mask.dng").as_slice(),
            include_str!("fixtures/generated-spatial2-mask.expected.json"),
        ),
    ] {
        let path = dir.path().join(format!("spatial{index}.dng"));
        std::fs::write(&path, bytes).unwrap();
        let image = decode_full(&path).unwrap();
        let expected: serde_json::Value = serde_json::from_str(oracle).unwrap();
        for y in 0..16 {
            for x in 0..36 {
                for c in 0..3 {
                    let target = expected["spatial_linear_srgb"][y][x][c].as_f64().unwrap() as f32;
                    assert!(
                        (image.pixels[y * 36 + x][c] - target).abs() < 0.002,
                        "PGTM{index} at{x},{y} c{c}: {} expected{target}",
                        image.pixels[y * 36 + x][c]
                    );
                }
                assert!(
                    (image.pixels[y * 36 + x][3]
                        - expected["alpha"][x % 6].as_f64().unwrap() as f32)
                        .abs()
                        < 1e-6
                );
            }
        }
        let applied = image.provenance.spatial_calibration.as_ref().unwrap();
        assert_eq!(applied.applied_pixels, 576);
        assert_eq!(applied.bypassed_pixels, 0);
        assert!(image.pixels.iter().any(|p| p[0] > 1.0));
        assert!(image.pixels.iter().any(|p| p[..3].iter().any(|c| *c < 0.0)));
        assert_eq!(image.pixels, decode_full(&path).unwrap().pixels);
    }
}

#[test]
fn psd_source_exif_strings_and_orientation_are_retained() {
    let dir = tempfile::tempdir().unwrap();
    // TIFF IFD with model ASCII and orientation, all generated metadata.
    let model = b"Camera verbatim model\0";
    let mut exif = b"II*\0\x08\0\0\0".to_vec();
    exif.extend(2u16.to_le_bytes());
    exif.extend(272u16.to_le_bytes());
    exif.extend(2u16.to_le_bytes());
    exif.extend((model.len() as u32).to_le_bytes());
    exif.extend(38u32.to_le_bytes());
    exif.extend(274u16.to_le_bytes());
    exif.extend(3u16.to_le_bytes());
    exif.extend(1u32.to_le_bytes());
    exif.extend(6u32.to_le_bytes());
    exif.extend(0u32.to_le_bytes());
    exif.extend(model);
    let mut resource = b"8BIM".to_vec();
    resource.extend(1058u16.to_be_bytes());
    resource.extend([0, 0]);
    resource.extend((exif.len() as u32).to_be_bytes());
    resource.extend(&exif);
    if !exif.len().is_multiple_of(2) {
        resource.push(0);
    }
    let mut bytes = b"8BPS\0\x01\0\0\0\0\0\0".to_vec();
    bytes.extend(3u16.to_be_bytes());
    bytes.extend(1u32.to_be_bytes());
    bytes.extend(2u32.to_be_bytes());
    bytes.extend(8u16.to_be_bytes());
    bytes.extend(3u16.to_be_bytes());
    bytes.extend(0u32.to_be_bytes());
    bytes.extend((resource.len() as u32).to_be_bytes());
    bytes.extend(resource);
    bytes.extend(0u32.to_be_bytes());
    bytes.extend(0u16.to_be_bytes());
    bytes.extend([255, 0, 0, 255, 0, 0]);
    let path = dir.path().join("source-exif.psd");
    std::fs::write(&path, bytes).unwrap();
    let image = decode_full(&path).unwrap();
    assert_eq!(
        image.metadata.camera_model.as_deref(),
        Some("Camera verbatim model")
    );
    assert_eq!((image.width, image.height), (1, 2));
    assert_eq!(image.metadata.orientation, 6);
}

#[test]
fn decode_admission_refuses_then_accepts_the_same_supported_sources() {
    use photocatalog::media::{DecodeLimits, decode_full_limited};
    let root = tempfile::tempdir().unwrap();
    let rgb = image::RgbImage::from_pixel(8, 6, image::Rgb([31, 89, 173]));
    let mut paths = Vec::new();
    for (extension, format) in [
        ("jpg", ImageFormat::Jpeg),
        ("png", ImageFormat::Png),
        ("webp", ImageFormat::WebP),
        ("bmp", ImageFormat::Bmp),
        ("tiff", ImageFormat::Tiff),
    ] {
        let path = root.path().join(format!("raster.{extension}"));
        rgb.save_with_format(&path, format).unwrap();
        paths.push(path);
    }
    for (name, bytes) in [
        (
            "mask.dng",
            include_bytes!("fixtures/generated-linear-mask.dng").as_slice(),
        ),
        (
            "swatch.avif",
            include_bytes!("fixtures/generated-swatches-10bit.avif").as_slice(),
        ),
    ] {
        let path = root.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        paths.push(path);
    }
    for path in paths {
        let source = std::fs::read(&path).unwrap();
        let refused = decode_full_limited(
            &path,
            DecodeLimits {
                max_intermediate_pixels: 1,
                ..DecodeLimits::default()
            },
        );
        assert_eq!(
            refused.err().unwrap().status,
            DecodeStatus::ResourceLimit,
            "{}",
            path.display()
        );
        let admitted = decode_full_limited(&path, DecodeLimits::default()).unwrap();
        assert!(admitted.pixels.len() > 1);
        assert_eq!(std::fs::read(&path).unwrap(), source);
    }
}

#[test]
fn encoded_admission_happens_before_codec_identification() {
    use photocatalog::media::{DecodeLimits, decode_full_limited};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("not-an-image");
    std::fs::write(&path, [0; 1024]).unwrap();
    let refused = decode_full_limited(
        &path,
        DecodeLimits {
            max_encoded_bytes: 128,
            ..DecodeLimits::default()
        },
    );
    assert_eq!(refused.err().unwrap().status, DecodeStatus::ResourceLimit);
    assert_eq!(
        decode_full(&path).err().unwrap().status,
        DecodeStatus::Unsupported
    );
}

#[test]
fn psd_surface_admission_precedes_missing_plane_payload() {
    let mut header = b"8BPS\0\x01\0\0\0\0\0\0".to_vec();
    header.extend(3u16.to_be_bytes());
    header.extend(1000u32.to_be_bytes());
    header.extend(1000u32.to_be_bytes());
    header.extend(16u16.to_be_bytes());
    header.extend(3u16.to_be_bytes());
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("header-only.psd");
    std::fs::write(&path, header).unwrap();
    let refused = photocatalog::media::decode_full_limited(
        &path,
        photocatalog::media::DecodeLimits {
            max_intermediate_pixels: 100,
            ..Default::default()
        },
    );
    assert_eq!(refused.err().unwrap().status, DecodeStatus::ResourceLimit);
    assert_eq!(
        decode_full(&path).err().unwrap().status,
        DecodeStatus::Corrupt
    );
}

#[test]
#[ignore = "requires the pinned public Panasonic RAW fixture via PHOTOCATALOG_RESOURCE_RAW"]
fn public_panasonic_raw_admission_retry() {
    use photocatalog::media::{DecodeLimits, decode_full_limited};
    let path = std::path::PathBuf::from(
        std::env::var_os("PHOTOCATALOG_RESOURCE_RAW").expect("public RAW fixture path"),
    );
    let original = std::fs::read(&path).unwrap();
    let refused = decode_full_limited(
        &path,
        DecodeLimits {
            max_intermediate_pixels: 1,
            ..Default::default()
        },
    );
    let error = refused.err().unwrap();
    assert_eq!(error.status, DecodeStatus::ResourceLimit);
    assert!(error.message.contains("RAW sensor/development"));
    let image = decode_full_limited(&path, DecodeLimits::default()).unwrap();
    assert_eq!((image.width, image.height), (4592, 3448));
    assert_eq!(image.provenance.source_bits_per_channel, 12);
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn psd_rle_narrow_many_channel_table_stays_in_encoded_buffer() {
    use photocatalog::media::{DecodeLimits, decode_full_limited};
    let mut bytes = psd_test_header(1, 1000, 56, 8, 1);
    for _ in 0..56_000 {
        bytes.extend(2u16.to_be_bytes());
    }
    for _ in 0..56_000 {
        bytes.extend([0, 128]);
    }
    assert_eq!(bytes.len(), 224040);
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("narrow.psd");
    std::fs::write(&path, &bytes).unwrap();
    let image = decode_full_limited(
        &path,
        DecodeLimits {
            max_encoded_bytes: 256 * 1024,
            max_intermediate_pixels: 1000,
            max_allocation_bytes: 256 * 1024,
        },
    )
    .unwrap();
    assert_eq!((image.width, image.height), (1, 1000));
    assert!(
        image
            .pixels
            .iter()
            .all(|p| (p[0] - 0.2158605).abs() < 1e-6 && p[3] == 1.0)
    );
}
fn psd_test_header(
    width: u32,
    height: u32,
    channels: u16,
    depth: u16,
    compression: u16,
) -> Vec<u8> {
    let mut bytes = b"8BPS\0\x01\0\0\0\0\0\0".to_vec();
    bytes.extend(channels.to_be_bytes());
    bytes.extend(height.to_be_bytes());
    bytes.extend(width.to_be_bytes());
    bytes.extend(depth.to_be_bytes());
    bytes.extend(3u16.to_be_bytes());
    for _ in 0..3 {
        bytes.extend(0u32.to_be_bytes());
    }
    bytes.extend(compression.to_be_bytes());
    bytes
}
#[test]
fn psd_zip_exact_output_rejects_short_and_excess_planes() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("zip.psd");
    for size in [23, 24, 25] {
        let mut bytes = psd_test_header(4, 2, 3, 8, 2);
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&vec![128; size]).unwrap();
        bytes.extend(encoder.finish().unwrap());
        std::fs::write(&path, bytes).unwrap();
        let result = decode_full(&path);
        if size == 24 {
            assert_eq!(result.unwrap().pixels.len(), 8);
        } else {
            assert_eq!(result.err().unwrap().status, DecodeStatus::Corrupt);
        }
    }
}
