use super::*;
use crate::edit::{self, Recipe, RenderError, RenderLimits, RenderPurpose};
use std::io::Cursor;
fn image() -> edit::EditedLinearImage {
    let p = edit::fixture(
        3,
        2,
        vec![
            [0., 0.18, 1., 1.],
            [0.123456, 0.5, 0.8, 0.5],
            [2., -0.2, 0.3, 0.],
            [0.3, 0.4, 0.5, 1.],
            [0.6, 0.7, 0.8, 0.25],
            [1., 1., 1., 1.],
        ],
    );
    edit::render_recipe(
        &p,
        &Recipe::default().validate().unwrap(),
        RenderPurpose::ExportExact,
        RenderLimits::default(),
        &(),
    )
    .unwrap()
}
fn spec(format: OutputFormat) -> OutputSpec {
    OutputSpec {
        size: OutputSize::Original,
        format,
        profile: OutputProfile::LinearSrgb,
        alpha: AlphaPolicy::Preserve,
    }
}
fn bytes(
    i: &edit::EditedLinearImage,
    s: &OutputSpec,
    m: &ResolvedExportMetadata,
) -> (Vec<u8>, EncodingReport) {
    let mut sink = BoundedSeekWriter::new(Cursor::new(Vec::new()), 16 * 1024 * 1024).unwrap();
    let report = encode_export(i, s, m, &mut sink, EncodeLimits::default(), &()).unwrap();
    (sink.into_inner().into_inner(), report)
}
fn metadata() -> ResolvedExportMetadata {
    ResolvedExportMetadata{xmp:Some(r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:test="https://example.invalid/export/" test:retained="café &amp; original"/></rdf:RDF></x:xmpmeta>"#.into()),exif:SafeExif{make:Some("Fixture Make".into()),model:Some("Fixture Model".into()),lens:Some("Fixture Lens".into()),exposure_time:Some(Rational{numerator:1,denominator:125}),iso:Some(200),..Default::default()}}
}
#[test]
fn all_lossless_depths_profiles_alpha_and_safe_metadata_read_back() {
    let i = image();
    let m = metadata();
    for format in [
        OutputFormat::Png {
            depth: IntegerDepth::Eight,
        },
        OutputFormat::Png {
            depth: IntegerDepth::Sixteen,
        },
        OutputFormat::Tiff {
            depth: TiffDepth::Eight,
        },
        OutputFormat::Tiff {
            depth: TiffDepth::Sixteen,
        },
        OutputFormat::Tiff {
            depth: TiffDepth::Float32,
        },
    ] {
        let s = spec(format);
        let (b, r) = bytes(&i, &s, &m);
        assert_eq!(r.encoded_extent, b.len() as u64);
        assert_eq!(
            (r.output.width, r.output.height, r.output.channels),
            (3, 2, 4)
        );
        let decoded = image::load_from_memory(&b).unwrap().into_rgba32f();
        for (actual, expected) in decoded.pixels().zip(i.as_rendered().pixels.iter()) {
            for c in 0..4 {
                let target = if r.output.floating_point {
                    expected[c]
                } else {
                    expected[c].clamp(0., 1.)
                };
                let tolerance = if r.output.bits_per_sample == 8 {
                    1. / 255.
                } else {
                    2e-5
                };
                assert!(
                    (actual[c] - target).abs() <= tolerance,
                    "{format:?} channel{c}: {} vs{target}",
                    actual[c]
                );
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let extension = if matches!(format, OutputFormat::Png { .. }) {
            "png"
        } else {
            "tif"
        };
        let path = temp.path().join(format!("out.{extension}"));
        std::fs::write(&path, &b).unwrap();
        let packets = crate::xmp_packets::inspect(&path, &Default::default()).unwrap();
        assert!(
            packets
                .parse_inputs
                .iter()
                .any(|p| p.bytes == m.xmp.as_ref().unwrap().as_bytes())
        );
        let mut reader = std::io::BufReader::new(Cursor::new(&b));
        let exif = exif::Reader::new()
            .read_from_container(&mut reader)
            .unwrap();
        assert_eq!(
            exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                .unwrap()
                .value
                .get_uint(0),
            Some(1)
        );
        assert_eq!(
            exif.get_field(exif::Tag::PixelXDimension, exif::In::PRIMARY)
                .unwrap()
                .value
                .get_uint(0),
            Some(3)
        );
        assert_eq!(
            exif.get_field(exif::Tag::PhotographicSensitivity, exif::In::PRIMARY)
                .unwrap()
                .value
                .get_uint(0),
            Some(200)
        );
        if extension == "png" {
            let decoder = png::Decoder::new(Cursor::new(&b));
            let reader = decoder.read_info().unwrap();
            assert_eq!(
                blake3::hash(reader.info().icc_profile.as_ref().unwrap())
                    .to_hex()
                    .to_string(),
                r.output.icc_blake3
            );
        } else {
            let mut decoder = tiff::decoder::Decoder::new(Cursor::new(&b)).unwrap();
            let icc = decoder
                .get_tag_u8_vec(tiff::tags::Tag::Unknown(34675))
                .unwrap();
            assert_eq!(blake3::hash(&icc).to_hex().to_string(), r.output.icc_blake3);
            assert_eq!(
                decoder
                    .get_tag_unsigned::<u16>(tiff::tags::Tag::ExtraSamples)
                    .unwrap(),
                2
            );
        }
    }
}
#[test]
fn jpeg_extended_xmp_preserves_full_selected_packet_and_composites() {
    let i = image();
    let mut m = metadata();
    let big = "0123456789abcdef".repeat(10000);
    m.xmp = Some(format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:test="https://example.invalid/export/" test:large="{big}" test:small="retained"/></rdf:RDF></x:xmpmeta>"#
    ));
    let s = OutputSpec {
        format: OutputFormat::Jpeg { quality: 95 },
        alpha: AlphaPolicy::Composite {
            linear_rgb: [1.; 3],
        },
        profile: OutputProfile::Srgb,
        ..spec(OutputFormat::Jpeg { quality: 95 })
    };
    let (b, r) = bytes(&i, &s, &m);
    assert!(
        b.windows(35)
            .any(|w| w.starts_with(b"http://ns.adobe.com/xmp/extension/"))
    );
    assert_eq!(image::load_from_memory(&b).unwrap().width(), 3);
    assert_eq!(r.output.channels, 3);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("output.jpg");
    std::fs::write(&path, b).unwrap();
    let inspected = crate::xmp_packets::inspect(&path, &Default::default()).unwrap();
    let joined = inspected
        .parse_inputs
        .iter()
        .map(|p| String::from_utf8_lossy(&p.bytes).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains(&big));
    assert!(joined.contains("retained"));
}
#[test]
fn output_description_is_deterministic_and_rejects_unsafe_choices() {
    let i = image();
    let s = spec(OutputFormat::Tiff {
        depth: TiffDepth::Float32,
    });
    assert_eq!(
        describe_output(&i, &s).unwrap(),
        describe_output(&i, &s).unwrap()
    );
    let mut bad = s.clone();
    bad.profile = OutputProfile::Srgb;
    assert!(matches!(
        describe_output(&i, &bad),
        Err(RenderError::InvalidProfile(_))
    ));
    bad = s.clone();
    bad.format = OutputFormat::Jpeg { quality: 80 };
    assert!(describe_output(&i, &bad).is_err());
    let mut resize = s.clone();
    resize.size = OutputSize::Fit {
        width: 6,
        height: 4,
        allow_upscale: false,
    };
    assert_eq!(describe_output(&i, &resize).unwrap().width, 3);
    resize.size = OutputSize::Fit {
        width: 6,
        height: 4,
        allow_upscale: true,
    };
    assert_eq!(describe_output(&i, &resize).unwrap().width, 6);
}
#[test]
fn encoded_extent_and_cancel_return_typed_failure_with_partial_file() {
    let i = image();
    let s = spec(OutputFormat::Png {
        depth: IntegerDepth::Eight,
    });
    let mut sink = BoundedSeekWriter::new(Cursor::new(Vec::new()), 64).unwrap();
    assert!(matches!(
        encode_export(
            &i,
            &s,
            &Default::default(),
            &mut sink,
            EncodeLimits::default(),
            &()
        ),
        Err(RenderError::ResourceLimit {
            resource: "encoded extent",
            ..
        })
    ));
    assert!(sink.extent() <= 64);
    let canceled = std::sync::atomic::AtomicBool::new(true);
    let mut sink = BoundedSeekWriter::new(Cursor::new(Vec::new()), 100000).unwrap();
    assert!(matches!(
        encode_export(
            &i,
            &s,
            &Default::default(),
            &mut sink,
            EncodeLimits::default(),
            &canceled
        ),
        Err(RenderError::Canceled)
    ));
    assert_eq!(sink.extent(), 0);
}
#[test]
fn caller_profile_roundtrips_and_bad_icc_metadata_are_explicit() {
    let i = image();
    let mut s = spec(OutputFormat::Png {
        depth: IntegerDepth::Sixteen,
    });
    let (_, icc, _) = super::specification::output_profile(&OutputProfile::Srgb).unwrap();
    s.profile = OutputProfile::Icc { bytes: icc.clone() };
    let (b, r) = bytes(&i, &s, &Default::default());
    assert_eq!(r.output.icc_blake3, blake3::hash(&icc).to_hex().to_string());
    let mut decoder = png::Decoder::new(Cursor::new(b)).read_info().unwrap();
    let mut out = vec![0; decoder.output_buffer_size().unwrap()];
    decoder.next_frame(&mut out).unwrap();
    assert_eq!(
        &out[2..4],
        &((0.46135613f32 * 65535.).round() as u16).to_be_bytes()
    );
    s.profile = OutputProfile::Icc {
        bytes: b"invalid".to_vec(),
    };
    assert!(describe_output(&i, &s).is_err());
    let invalid = ResolvedExportMetadata {
        exif: SafeExif {
            make: Some("bad\0value".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(invalid.validate(1024).is_err());
}
