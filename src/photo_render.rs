//! Pure exact-photo rendering into a caller-owned staging directory. Catalog job
//! ownership, destination identity, overwrite approval and publication are outside
//! this module. A failed staging file is retained, never published or removed here.
use crate::{
    edit::{self, CancelCheck, OriginalRequest, Recipe, RenderError, RenderLimits, RenderPurpose},
    image_export::{
        self, BoundedSeekWriter, EncodeLimits, EncodingReport, OutputFormat, OutputProfile,
        OutputSpec, Rational, ResolvedExportMetadata, SafeExif,
    },
    media::DecodeLimits,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::OnceLock,
};
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhotoRenderLimits {
    pub decode: DecodeLimits,
    pub render: RenderLimits,
    pub encode: EncodeLimits,
    pub max_encoded_extent: u64,
}
pub struct PhotoRenderRequest<'a> {
    pub original: &'a Path,
    pub expected_fingerprint: &'a str,
    pub recipe: &'a Recipe,
    pub output: &'a OutputSpec,
    pub selected_xmp: Option<&'a [u8]>,
    pub staging: &'a Path,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct StagedPhoto {
    pub staging: PathBuf,
    pub encoding: EncodingReport,
    pub renderer_identity: String,
    pub metadata_notes: Vec<String>,
}
/// Separate from the developed-proxy identity: changing a codec, metadata policy
/// or staging adapter invalidates export work without requiring a new RAW proxy.
pub fn output_renderer_identity() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let mut h = blake3::Hasher::new();
        for b in [
            include_bytes!("photo_render.rs").as_slice(),
            include_bytes!("image_export/encode.rs").as_slice(),
            include_bytes!("image_export/metadata.rs").as_slice(),
            include_bytes!("image_export/specification.rs").as_slice(),
            include_bytes!("image_export/sink.rs").as_slice(),
            include_bytes!("xmp.rs").as_slice(),
            include_bytes!("../Cargo.lock").as_slice(),
            include_bytes!("../vendor/xmp_toolkit/src/ffi.cpp").as_slice(),
            include_bytes!("../vendor/xmp_toolkit/src/xmp_meta.rs").as_slice(),
            edit::renderer_identity().as_bytes(),
        ] {
            h.update(&(b.len() as u64).to_le_bytes());
            h.update(b);
        }
        format!("photocatalog-photo-export-1:{}", h.finalize().to_hex())
    })
}
fn source_matches(
    path: &Path,
    expected: &str,
    limit: u64,
    cancel: &dyn CancelCheck,
) -> Result<(), RenderError> {
    let mut file = File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            RenderError::SourceMissing
        } else {
            e.into()
        }
    })?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(RenderError::InvalidInput(
            "original must be an ordinary file".into(),
        ));
    }
    let mut remaining = metadata.len();
    if remaining > limit {
        return Err(RenderError::ResourceLimit {
            resource: "original bytes",
            required: remaining,
            limit,
        });
    }
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0u8; 65536];
    while remaining > 0 {
        cancel.check()?;
        let n = file.read(&mut buffer[..remaining.min(65536) as usize])?;
        if n == 0 {
            return Err(RenderError::SourceChanged);
        }
        hash.update(&buffer[..n]);
        remaining -= n as u64;
    }
    let mut extra = [0];
    if file.read(&mut extra)? != 0 || hash.finalize().to_hex().as_str() != expected {
        return Err(RenderError::SourceChanged);
    }
    cancel.check()
}
pub fn render_staged_photo(
    request: PhotoRenderRequest<'_>,
    limits: PhotoRenderLimits,
    cancel: &dyn CancelCheck,
) -> Result<StagedPhoto, RenderError> {
    cancel.check()?;
    if !request.original.is_absolute() || !request.staging.is_absolute() {
        return Err(RenderError::InvalidInput(
            "absolute original/staging paths required".into(),
        ));
    }
    let recipe = request.recipe.validate()?;
    if let Some(packet) = request.selected_xmp {
        let limit = limits
            .encode
            .max_metadata_bytes
            .min(crate::xmp::MAX_PACKET_BYTES as u64);
        if packet.len() as u64 > limit {
            return Err(RenderError::ResourceLimit {
                resource: "selected XMP bytes",
                required: packet.len() as u64,
                limit,
            });
        }
    }
    if limits.max_encoded_extent == 0 {
        return Err(RenderError::ResourceLimit {
            resource: "encoded extent",
            required: 1,
            limit: 0,
        });
    }
    let original_limit = limits
        .decode
        .max_encoded_bytes
        .min(limits.decode.max_allocation_bytes);
    source_matches(
        request.original,
        request.expected_fingerprint,
        original_limit,
        cancel,
    )?;
    let original = request.original.canonicalize()?;
    let name = request
        .staging
        .file_name()
        .ok_or_else(|| RenderError::InvalidOutput("staging file name required".into()))?;
    let parent = request
        .staging
        .parent()
        .ok_or_else(|| RenderError::InvalidOutput("staging parent required".into()))?
        .canonicalize()?;
    let staging = parent.join(name);
    if staging == original {
        return Err(RenderError::InvalidOutput(
            "staging aliases the original".into(),
        ));
    }
    // create_new rejects existing files and final-component symlinks. The caller
    // owns and guards the staging directory identity for the whole worker lease.
    let file = OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .open(&staging)?;
    let mut sink = BoundedSeekWriter::new(file, limits.max_encoded_extent)?;
    let input = edit::decode_original(
        OriginalRequest {
            path: &original,
            expected_fingerprint: request.expected_fingerprint,
            white_balance: &recipe.settings().white_balance,
        },
        limits.decode,
        cancel,
    )?;
    let edited = edit::render_recipe(
        &input,
        &recipe,
        RenderPurpose::ExportExact,
        limits.render,
        cancel,
    )?;
    drop(input);
    let descriptor = image_export::describe_output(&edited, request.output)?;
    let mime = match request.output.format {
        OutputFormat::Jpeg { .. } => "image/jpeg",
        OutputFormat::Png { .. } => "image/png",
        OutputFormat::Tiff { .. } => "image/tiff",
    };
    let profile_name = match request.output.profile {
        OutputProfile::Srgb => "sRGB IEC61966-2.1".into(),
        OutputProfile::LinearSrgb => "linear sRGB".into(),
        OutputProfile::Icc { .. } => format!("ICC BLAKE3 {}", descriptor.icc_blake3),
    };
    let (metadata, metadata_notes) = derive_metadata(
        request.selected_xmp,
        crate::xmp::DerivativeFields {
            width: descriptor.width,
            height: descriptor.height,
            channels: descriptor.channels,
            bits_per_sample: descriptor.bits_per_sample,
            mime_type: mime.into(),
            profile_name,
            is_srgb: matches!(request.output.profile, OutputProfile::Srgb),
        },
    )?;
    cancel.check()?;
    let encoding = image_export::encode_export(
        &edited,
        request.output,
        &metadata,
        &mut sink,
        limits.encode,
        cancel,
    )?;
    source_matches(
        &original,
        request.expected_fingerprint,
        original_limit,
        cancel,
    )?;
    sink.into_inner().sync_all()?;
    cancel.check()?;
    Ok(StagedPhoto {
        staging,
        encoding,
        renderer_identity: output_renderer_identity().into(),
        metadata_notes,
    })
}
fn metadata_error(e: impl std::fmt::Display) -> RenderError {
    RenderError::InvalidMetadata(e.to_string())
}
const TIFF: &str = "http://ns.adobe.com/tiff/1.0/";
const EXIF: &str = "http://ns.adobe.com/exif/1.0/";
const AUX: &str = "http://ns.adobe.com/exif/1.0/aux/";
const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
/// Derive duplicate safe EXIF values only when the selected RDF has one literal
/// representation. Unrepresentable values remain in the full XMP and are named
/// in the receipt; no language, author or conflicting camera value is chosen.
fn derive_metadata(
    selected: Option<&[u8]>,
    fields: crate::xmp::DerivativeFields,
) -> Result<(ResolvedExportMetadata, Vec<String>), RenderError> {
    let empty;
    let base = match selected {
        Some(b) => b,
        None => {
            empty = crate::xmp::empty_packet().map_err(metadata_error)?;
            &empty
        }
    };
    let meta = crate::xmp::parse(base).map_err(metadata_error)?;
    let xml = crate::xmp::canonical(&meta).map_err(metadata_error)?;
    let doc = roxmltree::Document::parse(&xml).map_err(metadata_error)?;
    let mut notes = Vec::new();
    let mut text = |name: &str, addresses: &[(&str, &str)]| -> Option<String> {
        let mut values = Vec::new();
        let mut present = false;
        for &(ns, local) in addresses {
            for node in doc.descendants().filter(|n| {
                n.has_tag_name((ns, local))
                    && n.parent().is_some_and(|p| {
                        p.has_tag_name((RDF, "Description"))
                            && p.parent().is_some_and(|r| r.has_tag_name((RDF, "RDF")))
                    })
            }) {
                present = true;
                match one_literal(node) {
                    Some(s) => values.push(s),
                    None => {
                        notes.push(format!(
                            "{name}: structured/multiple values retained only in XMP"
                        ));
                        return None;
                    }
                }
            }
        }
        if !present {
            return None;
        }
        values.dedup();
        if values.len() != 1 {
            notes.push(format!("{name}: conflicting values retained only in XMP"));
            return None;
        }
        let value = values.pop().unwrap();
        if value.len() > 8192 || value.bytes().any(|b| b == 0 || !b.is_ascii()) {
            notes.push(format!(
                "{name}: Unicode/oversized value retained only in XMP"
            ));
            return None;
        }
        Some(value)
    };
    let make = text("make", &[(TIFF, "Make")]);
    let model = text("model", &[(TIFF, "Model")]);
    let lens = text(
        "lens",
        &[(AUX, "Lens"), ("http://cipa.jp/exif/1.0/", "LensModel")],
    );
    let artist = text("artist", &[(TIFF, "Artist"), (crate::xmp::DC, "creator")]);
    let copyright = text(
        "copyright",
        &[(TIFF, "Copyright"), (crate::xmp::DC, "rights")],
    );
    let description = text(
        "description",
        &[(TIFF, "ImageDescription"), (crate::xmp::DC, "description")],
    );
    let date = text("capture date", &[(EXIF, "DateTimeOriginal")]);
    let exposure = text("exposure time", &[(EXIF, "ExposureTime")]);
    let number = text("f number", &[(EXIF, "FNumber")]);
    let focal = text("focal length", &[(EXIF, "FocalLength")]);
    let sensitivity = text(
        "ISO",
        &[
            (EXIF, "ISOSpeedRatings"),
            ("http://cipa.jp/exif/1.0/", "PhotographicSensitivity"),
        ],
    );
    let date_time_original = date.and_then(|v| match capture_date(&v) {
        Some(s) => {if v.len()>19 {notes.push("capture date: subsecond/timezone details retained in XMP; EXIF stores local whole seconds".into());}Some(s)},
        None => {
            notes.push("capture date: invalid/unrepresentable date retained only in XMP".into());
            None
        }
    });
    let mut rational = |name: &str, value: Option<String>| {
        value.and_then(|v| match exact_rational(&v) {
            Some(r) => Some(r),
            None => {
                notes.push(format!(
                    "{name}: invalid/unrepresentable rational retained only in XMP"
                ));
                None
            }
        })
    };
    let exposure_time = rational("exposure time", exposure);
    let f_number = rational("f number", number);
    let focal_length = rational("focal length", focal);
    let iso = sensitivity.and_then(|v| match v.parse::<u16>() {
        Ok(n) if n > 0 => Some(n),
        _ => {
            notes.push("ISO: invalid/out-of-range value retained only in XMP".into());
            None
        }
    });
    let bytes = crate::xmp::rendered_derivative(base, &fields).map_err(metadata_error)?;
    let packet = String::from_utf8(bytes).map_err(metadata_error)?;
    Ok((
        ResolvedExportMetadata {
            xmp: Some(packet),
            exif: SafeExif {
                make,
                model,
                lens,
                date_time_original,
                artist,
                copyright,
                description,
                exposure_time,
                f_number,
                iso,
                focal_length,
            },
        },
        notes,
    ))
}
fn one_literal(node: roxmltree::Node<'_, '_>) -> Option<String> {
    if node.attribute((RDF, "resource")).is_some() {
        return None;
    }
    let children = node
        .children()
        .filter(|n| n.is_element())
        .collect::<Vec<_>>();
    if children.is_empty() {
        return Some(node.text().unwrap_or_default().to_owned());
    }
    if children.len() != 1 {
        return None;
    }
    let child = children[0];
    if child.has_tag_name((RDF, "value")) {
        return one_literal(child);
    }
    if child.has_tag_name((RDF, "Description")) {
        let mut values = child.children().filter(|n| n.has_tag_name((RDF, "value")));
        let value = values.next()?;
        if values.next().is_some() {
            return None;
        }
        return one_literal(value);
    }
    if ["Seq", "Bag", "Alt"]
        .iter()
        .any(|name| child.has_tag_name((RDF, *name)))
    {
        let mut values = child.children().filter(|n| n.is_element()).map(one_literal);
        let first = values.next()??;
        if values.all(|v| v.as_deref() == Some(first.as_str())) {
            return Some(first);
        }
        return None;
    }
    None
}
fn exact_rational(s: &str) -> Option<Rational> {
    let (mut n, mut d) = if let Some((a, b)) = s.split_once('/') {
        (a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)
    } else if let Some((a, b)) = s.split_once('.') {
        if b.len() > 9 || !b.bytes().all(|v| v.is_ascii_digit()) {
            return None;
        }
        let d = 10u64.checked_pow(b.len() as u32)?;
        (
            a.parse::<u64>()
                .ok()?
                .checked_mul(d)?
                .checked_add(b.parse::<u64>().ok()?)?,
            d,
        )
    } else {
        (s.parse::<u64>().ok()?, 1)
    };
    if n == 0 || d == 0 {
        return None;
    }
    let (mut a, mut b) = (n, d);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    n /= a;
    d /= a;
    Some(Rational {
        numerator: n.try_into().ok()?,
        denominator: d.try_into().ok()?,
    })
}
fn capture_date(s: &str) -> Option<String> {
    if s.len() < 19 || !s.is_ascii() {
        return None;
    }
    let b = s.as_bytes();
    if ![b':', b'-'].contains(&b[4])
        || b[7] != b[4]
        || ![b' ', b'T'].contains(&b[10])
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let number = |a: usize, z: usize| s[a..z].parse::<u32>().ok();
    let (year, month, day, hour, minute, second) = (
        number(0, 4)?,
        number(5, 7)?,
        number(8, 10)?,
        number(11, 13)?,
        number(14, 16)?,
        number(17, 19)?,
    );
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if year == 0 || day == 0 || day > days || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let tail = &s[19..];
    if !tail.is_empty() {
        let tail = if let Some(f) = tail.strip_prefix('.') {
            let n = f.bytes().take_while(u8::is_ascii_digit).count();
            if n == 0 {
                return None;
            }
            &f[n..]
        } else {
            tail
        };
        if tail != "Z"
            && !(tail.len() == 6
                && [b'+', b'-'].contains(&tail.as_bytes()[0])
                && tail.as_bytes()[3] == b':'
                && tail[1..3].parse::<u32>().is_ok_and(|n| n <= 23)
                && tail[4..6].parse::<u32>().is_ok_and(|n| n <= 59))
            && !tail.is_empty()
        {
            return None;
        }
    }
    Some(format!(
        "{year:04}:{month:02}:{day:02} {hour:02}:{minute:02}:{second:02}"
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fields() -> crate::xmp::DerivativeFields {
        crate::xmp::DerivativeFields {
            width: 3,
            height: 2,
            channels: 4,
            bits_per_sample: 16,
            mime_type: "image/png".into(),
            profile_name: "linear sRGB".into(),
            is_srgb: false,
        }
    }
    #[test]
    fn selected_metadata_keeps_unicode_and_conflicts_without_arbitrary_exif() {
        let base=br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:tiff="http://ns.adobe.com/tiff/1.0/" xmlns:exif="http://ns.adobe.com/exif/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" tiff:Make="Fixture" exif:ExposureTime="1/125" exif:FNumber="2.8" exif:DateTimeOriginal="2024-02-29T12:34:56-08:00" crs:Exposure2012="2"><dc:creator><rdf:Seq><rdf:li>A</rdf:li><rdf:li>B</rdf:li></rdf:Seq></dc:creator><dc:description><rdf:Alt><rdf:li xml:lang="x-default">caf&#233;</rdf:li></rdf:Alt></dc:description></rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let (before_hash, before) = (blake3::hash(base), base.to_vec());
        let (m, notes) = derive_metadata(Some(base), fields()).unwrap();
        assert_eq!(m.exif.make.as_deref(), Some("Fixture"));
        assert_eq!(
            m.exif.exposure_time,
            Some(Rational {
                numerator: 1,
                denominator: 125
            })
        );
        assert_eq!(
            m.exif.f_number,
            Some(Rational {
                numerator: 14,
                denominator: 5
            })
        );
        assert_eq!(
            m.exif.date_time_original.as_deref(),
            Some("2024:02:29 12:34:56")
        );
        assert!(m.exif.artist.is_none());
        assert!(m.exif.description.is_none());
        assert_eq!(notes.len(), 3);
        let xmp = m.xmp.unwrap();
        assert!(xmp.contains("caf"));
        assert!(!xmp.contains("Exposure2012"));
        assert_eq!(blake3::hash(&before), before_hash);
    }
    #[test]
    fn rational_and_date_conversion_are_exact_or_rejected() {
        assert_eq!(
            exact_rational("2.80"),
            Some(Rational {
                numerator: 14,
                denominator: 5
            })
        );
        assert!(exact_rational("1/0").is_none());
        assert!(exact_rational("-1/2").is_none());
        assert!(capture_date("2023-02-29T00:00:00Z").is_none());
        assert_eq!(
            capture_date("2024-02-29T12:34:56.123Z").as_deref(),
            Some("2024:02:29 12:34:56")
        );
        assert!(capture_date("2024-02-29T12:34:56garbage").is_none());
    }
    #[test]
    fn staging_is_exact_no_clobber_and_source_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.dng");
        let data = include_bytes!("../tests/fixtures/generated-linear-mask.dng");
        std::fs::write(&original, data).unwrap();
        let hash = blake3::hash(data).to_hex().to_string();
        let staging = dir.path().join("staging.png");
        let output = OutputSpec {
            size: image_export::OutputSize::Original,
            format: OutputFormat::Png {
                depth: image_export::IntegerDepth::Sixteen,
            },
            profile: OutputProfile::LinearSrgb,
            alpha: image_export::AlphaPolicy::Preserve,
        };
        let recipe = Recipe::default();
        let limits = PhotoRenderLimits {
            decode: DecodeLimits::default(),
            render: RenderLimits::default(),
            encode: EncodeLimits::default(),
            max_encoded_extent: 1_000_000,
        };
        let request = || PhotoRenderRequest {
            original: &original,
            expected_fingerprint: &hash,
            recipe: &recipe,
            output: &output,
            selected_xmp: None,
            staging: &staging,
        };
        let result = render_staged_photo(request(), limits, &()).unwrap();
        assert_eq!(result.encoding.output.bits_per_sample, 16);
        assert_eq!(result.encoding.output.width, 36);
        let encoded = std::fs::read(&staging).unwrap();
        assert!(render_staged_photo(request(), limits, &()).is_err());
        assert_eq!(std::fs::read(&staging).unwrap(), encoded);
        assert_eq!(std::fs::read(&original).unwrap(), data);
    }
}
