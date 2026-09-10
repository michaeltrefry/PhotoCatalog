//! Synthetic CFA DNG is sent directly to the LibRaw ABI, not the DNG SDK.
//! This tests the same native path used by CR2/RW2 without claiming a real camera.
use super::{DecodeLimits, NativeImage, NativeWhitePoint, pc_raw};

fn u16s(values: &[u16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn u32s(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn fixture(neutral: [f64; 3]) -> Vec<u8> {
    // Fixed IEC sRGB/D65 XYZ-to-RGB coefficients, independently specified.
    // ColorMatrix = diag(AsShotNeutral) * XYZ_to_sRGB makes the balanced
    // camera primaries sRGB. LibRaw may round its matrix, so allow 0.003 error.
    let xyz_to_rgb = [
        [3.2404542, -1.5371385, -0.4985314],
        [-0.9692660, 1.8760108, 0.0415560],
        [0.0556434, -0.2040259, 1.0572252],
    ];
    let mut matrix = Vec::new();
    for c in 0..3 {
        for value in xyz_to_rgb[c] {
            matrix.extend_from_slice(&((value * neutral[c] * 1e7).round() as i32).to_le_bytes());
            matrix.extend_from_slice(&10_000_000i32.to_le_bytes());
        }
    }
    let mut as_shot = Vec::new();
    for n in neutral {
        as_shot.extend_from_slice(&((n * 1000.0).round() as u32).to_le_bytes());
        as_shot.extend_from_slice(&1000u32.to_le_bytes());
    }
    // Low neutral exposure ramp; real colors below clipping; fully clipped
    // sensor white; bright red with an unclipped green/blue signal.
    let mut patches = [
        [0.125; 3],
        [0.25; 3],
        [0.5; 3],
        [0.8, 0.05, 0.02],
        [0.03, 0.7, 0.04],
        [0.02, 0.05, 0.8],
        [0.0; 3],
        [1.6, 0.2, 0.1],
    ];
    let primary = (0..3)
        .min_by(|a, b| neutral[*a].total_cmp(&neutral[*b]))
        .unwrap();
    patches[7] = [0.1; 3];
    patches[7][primary] = 1.6;
    let (width, height) = (512u32, 64u32);
    let mut pixels = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let patch = x as usize / 64;
            let c = match (y % 2, x % 2) {
                (0, 0) => 0,
                (1, 1) => 2,
                _ => 1,
            };
            let value = if patch == 6 {
                1.0
            } else {
                patches[patch][c] * neutral[c]
            };
            pixels
                .extend_from_slice(&(64 + (value.min(1.0) * 4031.0).round() as u16).to_le_bytes());
        }
    }
    let mut tags: Vec<(u16, u16, u32, Vec<u8>)> = vec![
        (254, 4, 1, u32s(&[0])),
        (256, 4, 1, u32s(&[width])),
        (257, 4, 1, u32s(&[height])),
        (258, 3, 1, u16s(&[16])),
        (259, 3, 1, u16s(&[1])),
        (262, 3, 1, u16s(&[32803])),
        (271, 2, 13, b"PhotoCatalog\0".to_vec()),
        (272, 2, 14, b"Analytic RGGB\0".to_vec()),
        (273, 4, 1, u32s(&[0])),
        (274, 3, 1, u16s(&[1])),
        (277, 3, 1, u16s(&[1])),
        (278, 4, 1, u32s(&[height])),
        (279, 4, 1, u32s(&[pixels.len() as u32])),
        (33421, 3, 2, u16s(&[2, 2])),
        (33422, 1, 4, vec![0, 1, 1, 2]),
        (50706, 1, 4, vec![1, 4, 0, 0]),
        (50707, 1, 4, vec![1, 1, 0, 0]),
        (50708, 2, 14, b"Analytic RGGB\0".to_vec()),
        (50713, 3, 2, u16s(&[1, 1])),
        (50714, 4, 1, u32s(&[64])),
        (50717, 4, 1, u32s(&[4095])),
        (50721, 10, 9, matrix),
        (50728, 5, 3, as_shot),
        (50778, 3, 1, u16s(&[21])),
    ];
    tags.sort_by_key(|t| t.0);
    let mut output = b"II\x2a\0\x08\0\0\0".to_vec();
    output.extend_from_slice(&(tags.len() as u16).to_le_bytes());
    output.resize(8 + 2 + tags.len() * 12 + 4, 0);
    let mut strip_field = 0;
    for (i, (tag, kind, count, value)) in tags.into_iter().enumerate() {
        let at = 10 + i * 12;
        output[at..at + 2].copy_from_slice(&tag.to_le_bytes());
        output[at + 2..at + 4].copy_from_slice(&kind.to_le_bytes());
        output[at + 4..at + 8].copy_from_slice(&count.to_le_bytes());
        if value.len() <= 4 {
            output[at + 8..at + 8 + value.len()].copy_from_slice(&value);
        } else {
            let offset = output.len() as u32;
            output[at + 8..at + 12].copy_from_slice(&offset.to_le_bytes());
            output.extend(value);
            if !output.len().is_multiple_of(2) {
                output.push(0);
            }
        }
        if tag == 273 {
            strip_field = at + 8;
        }
    }
    let offset = output.len() as u32;
    output[strip_field..strip_field + 4].copy_from_slice(&offset.to_le_bytes());
    output.extend(pixels);
    output
}

#[test]
fn raw_clipped_neutral_and_real_colors_preserve_linear_headroom() {
    // Different illuminants and which WB channel is largest prevent a
    // camera-specific or red/blue-only correction from satisfying the oracle.
    for neutral in [[0.5, 1.0, 0.4], [1.0, 0.5, 0.8], [0.8, 1.0, 0.5]] {
        let bytes = fixture(neutral);
        let before = blake3::hash(&bytes);
        let mut out: NativeImage = unsafe { std::mem::zeroed() };
        let status = unsafe {
            pc_raw(
                bytes.as_ptr(),
                bytes.len(),
                &DecodeLimits::default(),
                &NativeWhitePoint { x: 0.0, y: 0.0 },
                &mut out,
            )
        };
        assert_eq!(
            status,
            0,
            "{}",
            unsafe { std::ffi::CStr::from_ptr(out.error.as_ptr()) }.to_string_lossy()
        );
        assert_eq!(blake3::hash(&bytes), before);
        assert_eq!(
            (out.width, out.height, out.primaries, out.transfer),
            (512, 64, 1, 8)
        );
        let pixels = unsafe { std::slice::from_raw_parts(out.pixels, 512 * 64 * 4) };
        assert!(pixels.iter().all(|x| x.is_finite()));
        let at = |patch: usize| {
            let start = (32 * 512 + patch * 64 + 32) * 4;
            &pixels[start..start + 4]
        };
        for (patch, expected) in [
            (0, [0.125; 3]),
            (1, [0.25; 3]),
            (2, [0.5; 3]),
            (3, [0.8, 0.05, 0.02]),
            (4, [0.03, 0.7, 0.04]),
            (5, [0.02, 0.05, 0.8]),
        ] {
            for (c, value) in expected.iter().enumerate() {
                assert!(
                    (at(patch)[c] - value).abs() < 0.003,
                    "WB {neutral:?} patch{patch} channel{c}: {:?} expected {expected:?}",
                    at(patch)
                );
            }
            assert_eq!(at(patch)[3], 1.0);
        }
        let white = at(6);
        assert!(
            white[..3].iter().all(|x| *x > 1.0),
            "HDR clipped white: {white:?}"
        );
        assert!(
            (white[0] - white[1]).abs() < 0.003 && (white[1] - white[2]).abs() < 0.003,
            "false highlight chroma {neutral:?}: {white:?}"
        );
        let color = at(7);
        let primary = (0..3)
            .min_by(|a, b| neutral[*a].total_cmp(&neutral[*b]))
            .unwrap();
        assert!(
            color[primary] > 1.0
                && (0..3)
                    .filter(|c| *c != primary)
                    .all(|c| color[primary] > color[c] * 2.0),
            "bright color incorrectly neutralized: {color:?}"
        );
    }
}

#[test]
fn raw_sensor_admission_precedes_unpack() {
    let bytes = fixture([0.5, 1.0, 0.7]);
    let mut out: NativeImage = unsafe { std::mem::zeroed() };
    let limits = DecodeLimits {
        max_intermediate_pixels: 1,
        ..Default::default()
    };
    assert_ne!(
        unsafe {
            pc_raw(
                bytes.as_ptr(),
                bytes.len(),
                &limits,
                &NativeWhitePoint { x: 0.0, y: 0.0 },
                &mut out,
            )
        },
        0
    );
    assert!(out.pixels.is_null());
    let message = unsafe { std::ffi::CStr::from_ptr(out.error.as_ptr()) }.to_string_lossy();
    assert!(message.contains("resource limit: RAW sensor/development"));
    assert_eq!(
        unsafe {
            pc_raw(
                bytes.as_ptr(),
                bytes.len(),
                &DecodeLimits::default(),
                &NativeWhitePoint { x: 0.0, y: 0.0 },
                &mut out,
            )
        },
        0
    );
    assert!(!out.pixels.is_null());
}

#[test]
fn custom_raw_white_uses_camera_response_before_development() {
    let bytes = fixture([0.5, 1.0, 0.4]);
    // Independent illuminant A XYZ, multiplied by fixed IEC XYZ->sRGB.
    // For the fixture's diagonal camera model, each developed neutral patch
    // must have channel ratios inverse to this illuminant's RGB response.
    let (x, y) = (0.44757f64, 0.40745f64);
    let xyz = [x / y, 1.0, (1.0 - x - y) / y];
    let rgb = [
        [3.2404542, -1.5371385, -0.4985314],
        [-0.9692660, 1.8760108, 0.0415560],
        [0.0556434, -0.2040259, 1.0572252],
    ]
    .map(|r| r[0] * xyz[0] + r[1] * xyz[1] + r[2] * xyz[2]);
    let mut out: NativeImage = unsafe { std::mem::zeroed() };
    let status = unsafe {
        pc_raw(
            bytes.as_ptr(),
            bytes.len(),
            &DecodeLimits::default(),
            &NativeWhitePoint { x, y },
            &mut out,
        )
    };
    assert_eq!(
        status,
        0,
        "{}",
        unsafe { std::ffi::CStr::from_ptr(out.error.as_ptr()) }.to_string_lossy()
    );
    let pixels = unsafe {
        std::slice::from_raw_parts(out.pixels, out.width as usize * out.height as usize * 4)
    };
    let p = &pixels[(32 * out.width as usize + 96) * 4..][..3];
    assert!((f64::from(p[0] / p[1]) - rgb[1] / rgb[0]).abs() < 0.015);
    assert!((f64::from(p[2] / p[1]) - rgb[1] / rgb[2]).abs() < 0.03);
    drop(out);
}

/// This exercises the Adobe DNG SDK route separately from the LibRaw ABI above.
/// The fixture has one known ColorMatrix and no ForwardMatrix/look tables. A
/// camera sample proportional to ColorMatrix * XYZ(illuminant), normalized to its
/// largest channel, must develop to neutral under that requested illuminant.
/// Expected sensor responses use fixed IEC coefficients, not the product's WB
/// implementation or the SDK's computed matrix.
#[test]
fn requested_dng_white_maps_independent_camera_neutrals_to_gray() {
    let neutral = [0.5, 1.0, 0.4];
    let mut bytes = fixture(neutral);
    let whites = [(0.44757f64, 0.40745f64), (0.3127, 0.3290)];
    let rgb_from_xyz = [
        [3.2404542, -1.5371385, -0.4985314],
        [-0.9692660, 1.8760108, 0.0415560],
        [0.0556434, -0.2040259, 1.0572252],
    ];
    let responses = whites.map(|(x, y)| {
        let xyz = [x / y, 1.0, (1.0 - x - y) / y];
        let camera: [f64; 3] = std::array::from_fn(|c| {
            neutral[c]
                * (rgb_from_xyz[c][0] * xyz[0]
                    + rgb_from_xyz[c][1] * xyz[1]
                    + rgb_from_xyz[c][2] * xyz[2])
        });
        let peak = camera.into_iter().fold(0.0f64, f64::max);
        camera.map(|c| c / peak)
    });
    // The fixture's one uncompressed strip is last. Two constant 64x64 Bayer
    // patches leave their centers far from both image and interpolation borders.
    let offset = bytes.len() - 512 * 64 * 2;
    for y in 0..64usize {
        for x in 0..128usize {
            let channel = match (y % 2, x % 2) {
                (0, 0) => 0,
                (1, 1) => 2,
                _ => 1,
            };
            let sensor = 64 + (0.25 * responses[x / 64][channel] * 4031.0).round() as u16;
            let at = offset + (y * 512 + x) * 2;
            bytes[at..at + 2].copy_from_slice(&sensor.to_le_bytes());
        }
    }
    let unchanged = blake3::hash(&bytes);
    for (selected, (x, y)) in whites.into_iter().enumerate() {
        let mut out: NativeImage = unsafe { std::mem::zeroed() };
        let status = unsafe {
            super::pc_dng(
                bytes.as_ptr(),
                bytes.len(),
                &DecodeLimits::default(),
                &NativeWhitePoint { x, y },
                &mut out,
            )
        };
        assert_eq!(
            status,
            0,
            "{}",
            unsafe { std::ffi::CStr::from_ptr(out.error.as_ptr()) }.to_string_lossy()
        );
        assert_eq!(
            (out.width, out.height, out.primaries, out.transfer),
            (512, 64, 1, 8)
        );
        let pixels = unsafe { std::slice::from_raw_parts(out.pixels, 512 * 64 * 4) };
        let patch = |i: usize| &pixels[(32 * 512 + i * 64 + 32) * 4..][..4];
        let actual = patch(selected);
        // Prospective bound includes 12-bit sensor rounding and integer stage2/3.
        // It is deliberately independent of the previous LibRaw ratio bounds.
        for value in &actual[..3] {
            assert!(
                (*value - 0.25).abs() < 0.003,
                "requested DNG white {selected}: neutral patch {actual:?}"
            );
        }
        assert_eq!(actual[3], 1.0);
        let other = patch(1 - selected);
        let high = other[..3].iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let low = other[..3].iter().copied().fold(f32::INFINITY, f32::min);
        assert!(
            high - low > 0.05,
            "different illuminant must remain chromatic: {other:?}"
        );
        assert_eq!(blake3::hash(&bytes), unchanged);
    }
}
