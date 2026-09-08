use super::{Metadata, orient};
use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use lcms2::{CIExyY, CIExyYTRIPLE, Flags, Intent, PixelFormat, Profile, ToneCurve, Transform};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, c_char},
    fmt,
    fs::File,
    io::{BufReader, Cursor, Read},
    path::Path,
};

const MAX_ENCODED: u64 = 512 * 1024 * 1024;
const MAX_PIXELS: u64 = 100_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeStatus {
    Unsupported,
    Corrupt,
    ResourceLimit,
    Io,
}
#[derive(Debug)]
pub struct DecodeError {
    pub status: DecodeStatus,
    pub message: String,
}
impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.status, self.message)
    }
}
impl std::error::Error for DecodeError {}
type Result<T> = std::result::Result<T, DecodeError>;
fn error(status: DecodeStatus, message: impl ToString) -> DecodeError {
    DecodeError {
        status,
        message: message.to_string(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderProvenance {
    pub pipeline_version: String,
    pub decoder: String,
    pub source_bits_per_channel: u32,
    pub source_color: String,
    pub working_color: String,
    pub alpha: String,
    pub notes: Vec<String>,
}
/// Full-resolution, oriented, scene/display-linear sRGB-primary pixels. RGB may be
/// negative or above one; alpha is straight and in [0,1]. No thumbnail dependency.
pub struct RenderedImage {
    pub metadata: Metadata,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<[f32; 4]>,
    pub provenance: RenderProvenance,
}
impl RenderedImage {
    /// Browse-only SDR conversion. The float editor input remains unchanged.
    pub fn srgb_preview(&self, edge: u32) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!((1..=8192).contains(&edge), "preview edge outside 1..8192");
        let mut rgba = RgbaImage::new(self.width, self.height);
        for (target, source) in rgba.pixels_mut().zip(&self.pixels) {
            for c in 0..3 {
                // Composite straight alpha against a neutral white canvas in linear light.
                let v = source[c] * source[3] + 1.0 - source[3];
                target[c] = (linear_to_srgb(v).clamp(0.0, 1.0) * 255.0).round() as u8;
            }
            target[3] = 255;
        }
        let image = DynamicImage::ImageRgba8(rgba)
            .thumbnail(edge, edge)
            .to_rgb8();
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 65).encode_image(&image)?;
        Ok(bytes)
    }
}
pub fn linear_to_srgb(x: f32) -> f32 {
    if x <= 0.0031308 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}
pub fn srgb_to_linear(x: f32) -> f32 {
    if x <= 0.04045 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

#[repr(C)]
struct NativeImage {
    pixels: *mut f32,
    icc: *mut u8,
    icc_size: usize,
    width: u32,
    height: u32,
    bits: u32,
    orientation: u32,
    primaries: u32,
    transfer: u32,
    flags: u32,
    make: [c_char; 128],
    model: [c_char; 128],
    profile: [c_char; 128],
    error: [c_char; 256],
}
unsafe extern "C" {
    fn pc_raw(bytes: *const u8, len: usize, out: *mut NativeImage) -> i32;
    fn pc_dng(bytes: *const u8, len: usize, out: *mut NativeImage) -> i32;
    fn pc_avif(bytes: *const u8, len: usize, out: *mut NativeImage) -> i32;
    fn pc_free(out: *mut NativeImage);
    fn pc_raw_version() -> *const c_char;
    fn pc_avif_version() -> *const c_char;
}
impl Drop for NativeImage {
    fn drop(&mut self) {
        unsafe { pc_free(self) };
    }
}
pub fn decoder_versions() -> String {
    // Vendor functions return immutable NUL-terminated static strings.
    unsafe {
        format!(
            "Adobe DNG SDK 1.7.1 (2724); LibRaw {}; libavif {}; LittleCMS {}",
            CStr::from_ptr(pc_raw_version()).to_string_lossy(),
            CStr::from_ptr(pc_avif_version()).to_string_lossy(),
            lcms2::version()
        )
    }
}

pub fn decode_full(path: &Path) -> Result<RenderedImage> {
    let file = File::open(path).map_err(|e| error(DecodeStatus::Io, e))?;
    if file
        .metadata()
        .map_err(|e| error(DecodeStatus::Io, e))?
        .len()
        > MAX_ENCODED
    {
        return Err(error(
            DecodeStatus::ResourceLimit,
            "encoded image exceeds 512 MiB",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_ENCODED + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| error(DecodeStatus::Io, e))?;
    if bytes.len() as u64 > MAX_ENCODED {
        return Err(error(
            DecodeStatus::ResourceLimit,
            "source grew beyond limit",
        ));
    }
    let ext = path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let exif = exif::Reader::new()
        .read_from_container(&mut BufReader::new(Cursor::new(&bytes)))
        .ok();
    let text = |tag| {
        exif.as_ref()
            .and_then(|x| x.get_field(tag, exif::In::PRIMARY))
            .map(|f| f.display_value().to_string())
    };
    let exif_orientation = exif
        .as_ref()
        .and_then(|x| x.get_field(exif::Tag::Orientation, exif::In::PRIMARY))
        .and_then(|f| f.value.get_uint(0));
    let raw_ext = [
        "cr2", "cr3", "dng", "raf", "rw2", "nef", "arw", "orf", "pef",
    ]
    .contains(&ext.as_str());
    let is_raw =
        raw_ext || bytes.get(8..12) == Some(b"CR\x02\0") || bytes.starts_with(b"FUJIFILMCCD-RAW");
    let is_avif = bytes.get(4..8) == Some(b"ftyp")
        && bytes
            .get(8..64.min(bytes.len()))
            .is_some_and(|b| b.windows(4).any(|x| x == b"avif" || x == b"avis"));
    let (mut image, format, bits, icc, orientation, source_color, decoder, make, model, mut notes) =
        if is_raw || is_avif {
            // Zeroed pointers and lengths form an empty native result; Drop frees all native allocations on every path.
            let mut out: NativeImage = unsafe { std::mem::zeroed() };
            let status = unsafe {
                if ext == "dng" {
                    pc_dng(bytes.as_ptr(), bytes.len(), &mut out)
                } else if is_raw {
                    pc_raw(bytes.as_ptr(), bytes.len(), &mut out)
                } else {
                    pc_avif(bytes.as_ptr(), bytes.len(), &mut out)
                }
            };
            if status != 0 {
                let message = unsafe { CStr::from_ptr(out.error.as_ptr()) }.to_string_lossy();
                let kind = if message.contains("resource limit") {
                    DecodeStatus::ResourceLimit
                } else if message.contains("unsupported") || message.contains("Unsupported") {
                    DecodeStatus::Unsupported
                } else {
                    DecodeStatus::Corrupt
                };
                return Err(error(kind, message));
            }
            dimensions(out.width, out.height)?;
            if out.pixels.is_null() {
                return Err(error(
                    DecodeStatus::Corrupt,
                    "native decoder returned no pixels",
                ));
            }
            let samples = unsafe {
                std::slice::from_raw_parts(
                    out.pixels,
                    (out.width as usize) * (out.height as usize) * 4,
                )
            }
            .to_vec();
            let buffer = image::Rgba32FImage::from_raw(out.width, out.height, samples)
                .ok_or_else(|| error(DecodeStatus::Corrupt, "native dimension mismatch"))?;
            let icc = if out.icc_size == 0 {
                None
            } else {
                Some(unsafe { std::slice::from_raw_parts(out.icc, out.icc_size) }.to_vec())
            };
            let label = if is_raw {
                "linear sRGB camera matrix".to_string()
            } else {
                format!("CICP primaries {} transfer {}", out.primaries, out.transfer)
            };
            let make = unsafe { CStr::from_ptr(out.make.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let model = unsafe { CStr::from_ptr(out.model.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let mut dynamic = DynamicImage::ImageRgba32F(buffer);
            if is_avif && icc.is_none() {
                convert_cicp(&mut dynamic, out.primaries, out.transfer)?;
            }
            let mut notes = Vec::new();
            if ext == "dng" {
                let profile = unsafe { CStr::from_ptr(out.profile.as_ptr()) }.to_string_lossy();
                notes.push(format!("Adobe DNG SDK 1.7.1 original RAW stage3; camera profile matrix '{profile}', as-shot white balance, default crop and transparency mask. Display tone curves are excluded from scene-linear editor input."));
                if out.flags & 1 != 0 {
                    notes.push("Floating-point DNG sample precision retained through normalization and camera color transform.".into());
                }
                if out.flags & 2 != 0 {
                    notes.push("Source contains a separate enhanced rendition; original RAW is selected. Enhanced/AI appearance is not reproduced.".into());
                }
                if out.flags & 4 != 0 {
                    notes.push("Source profile hue/saturation calibration table is retained in original but is not applied by this matrix-only working-color transform.".into());
                }
                if out.flags & 8 != 0 {
                    notes.push("Source profile LookTable is retained in original but not applied to scene-linear editor input.".into());
                }
            } else if is_raw {
                notes.push("Camera matrix and as-shot white balance; Adobe DCP looks, local instructions and DNG opcode rendering are not reproduced.".into());
                notes.push(if out.bits==32 {"Floating linear DNG: no integer clipping; matrix/WB interpretation requires camera reference validation."} else {"LibRaw full-size camera RGB demosaic with unit WB, linear 16-bit sensor normalization; as-shot WB and camera-to-sRGB matrix applied afterwards in unclamped float. Sensor samples above white are clipped before demosaic."}.into());
            }
            (
                dynamic,
                if is_raw {
                    ext.to_ascii_uppercase()
                } else {
                    "AVIF".into()
                },
                out.bits,
                icc,
                exif_orientation.unwrap_or(out.orientation),
                label,
                decoder_versions(),
                (!make.is_empty()).then_some(make),
                (!model.is_empty()).then_some(model),
                notes,
            )
        } else if bytes.starts_with(b"8BPS") {
            let decoded = super::psd::decode(&bytes).map_err(|e| {
                let msg = e.to_string();
                error(
                    if msg.contains("unsupported") {
                        DecodeStatus::Unsupported
                    } else if msg.contains("limit") {
                        DecodeStatus::ResourceLimit
                    } else {
                        DecodeStatus::Corrupt
                    },
                    msg,
                )
            })?;
            (
                decoded.image,
                "PSD".into(),
                decoded.bits,
                decoded.icc,
                exif_orientation.unwrap_or(1),
                "embedded ICC or untagged sRGB".into(),
                "bounded PSD composite".into(),
                None,
                None,
                vec!["Merged composite only; layers are retained in the original.".into()],
            )
        } else {
            let format =
                image::guess_format(&bytes).map_err(|e| error(DecodeStatus::Unsupported, e))?;
            if !matches!(
                format,
                image::ImageFormat::Jpeg
                    | image::ImageFormat::Png
                    | image::ImageFormat::WebP
                    | image::ImageFormat::Bmp
                    | image::ImageFormat::Tiff
            ) {
                return Err(error(DecodeStatus::Unsupported, "unsupported raster codec"));
            }
            let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(40000);
            limits.max_image_height = Some(40000);
            limits.max_alloc = Some(1600 * 1024 * 1024);
            reader.limits(limits);
            let mut decoder = reader.into_decoder().map_err(image_error)?;
            dimensions(decoder.dimensions().0, decoder.dimensions().1)?;
            let mut icc = decoder.icc_profile().map_err(image_error)?;
            if format == image::ImageFormat::Png && icc.is_none() {
                icc = png_profile(&bytes)?;
            }
            let orientation = exif_orientation
                .unwrap_or(decoder.orientation().map_err(image_error)?.to_exif() as u32);
            let color = decoder.original_color_type();
            let bits = color.bits_per_pixel() as u32 / u32::from(color.channel_count());
            let image = DynamicImage::from_decoder(decoder).map_err(image_error)?;
            let format = match format {
                image::ImageFormat::Jpeg => "JPEG",
                image::ImageFormat::Png => "PNG",
                image::ImageFormat::WebP => "WEBP",
                image::ImageFormat::Bmp => "BMP",
                _ => "TIFF",
            };
            (
                image,
                format.into(),
                bits,
                icc,
                orientation,
                "embedded ICC or untagged sRGB".into(),
                "image-rs".into(),
                None,
                None,
                Vec::new(),
            )
        };
    if !(1..=8).contains(&orientation) {
        return Err(error(DecodeStatus::Corrupt, "invalid orientation"));
    }
    dimensions(image.width(), image.height())?;
    let (width, height) = (image.width(), image.height());
    let already_linear = (is_raw || is_avif) && icc.is_none();
    if !already_linear {
        image = to_linear(image, icc.as_deref())?;
    }
    if icc.is_none() && !already_linear {
        notes.push(
            "No embedded ICC: assumes sRGB; assignment is recorded, not inferred from filename."
                .into(),
        );
    }
    let image = orient(image, orientation).into_rgba32f();
    let (out_width, out_height) = image.dimensions();
    let pixels: Vec<[f32; 4]> = image.into_raw().as_chunks::<4>().0.to_vec();
    if pixels
        .iter()
        .any(|p| p.iter().any(|x| !x.is_finite()) || !(0.0..=1.0).contains(&p[3]))
    {
        return Err(error(
            DecodeStatus::Corrupt,
            "non-finite color or invalid alpha",
        ));
    }
    Ok(RenderedImage {
        metadata: Metadata {
            format,
            width,
            height,
            orientation,
            camera_make: make.or_else(|| text(exif::Tag::Make)),
            camera_model: model.or_else(|| text(exif::Tag::Model)),
            captured_at: text(exif::Tag::DateTimeOriginal),
            preview_source: "full-quality original rendering".into(),
        },
        width: out_width,
        height: out_height,
        pixels,
        provenance: RenderProvenance {
            pipeline_version: "photocatalog-render-1".into(),
            decoder,
            source_bits_per_channel: bits,
            source_color,
            working_color: "linear sRGB primaries, D65, f32, unclamped".into(),
            alpha: "straight alpha [0,1]".into(),
            notes,
        },
    })
}
fn image_error(e: image::ImageError) -> DecodeError {
    let status = match e {
        image::ImageError::Unsupported(_) => DecodeStatus::Unsupported,
        image::ImageError::Limits(_) => DecodeStatus::ResourceLimit,
        _ => DecodeStatus::Corrupt,
    };
    error(status, e)
}
fn dimensions(w: u32, h: u32) -> Result<()> {
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        Err(error(
            DecodeStatus::ResourceLimit,
            "image exceeds 100 megapixels",
        ))
    } else {
        Ok(())
    }
}

pub fn linear_profile() -> std::result::Result<Profile, lcms2::Error> {
    let curve = ToneCurve::new(1.0);
    Profile::new_rgb(
        &CIExyY {
            x: 0.3127,
            y: 0.3290,
            Y: 1.0,
        },
        &CIExyYTRIPLE {
            Red: CIExyY {
                x: 0.64,
                y: 0.33,
                Y: 1.0,
            },
            Green: CIExyY {
                x: 0.30,
                y: 0.60,
                Y: 1.0,
            },
            Blue: CIExyY {
                x: 0.15,
                y: 0.06,
                Y: 1.0,
            },
        },
        &[&curve, &curve, &curve],
    )
}
fn to_linear(image: DynamicImage, icc: Option<&[u8]>) -> Result<DynamicImage> {
    let mut buffer = image.to_rgba32f();
    if let Some(icc) = icc {
        if icc.len() > 16 * 1024 * 1024 {
            return Err(error(
                DecodeStatus::ResourceLimit,
                "ICC profile exceeds 16 MiB",
            ));
        }
        let source = Profile::new_icc(icc).map_err(|e| error(DecodeStatus::Corrupt, e))?;
        if source.color_space() != lcms2::ColorSpaceSignature::RgbData {
            return Err(error(
                DecodeStatus::Unsupported,
                "non-RGB ICC profile requires matching source channel transform",
            ));
        }
        let target = linear_profile().map_err(|e| error(DecodeStatus::Corrupt, e))?;
        let transform: Transform<[f32; 4], [f32; 4]> = Transform::new_flags(
            &source,
            PixelFormat::RGBA_FLT,
            &target,
            PixelFormat::RGBA_FLT,
            Intent::RelativeColorimetric,
            Flags::COPY_ALPHA | Flags::NO_OPTIMIZE,
        )
        .map_err(|e| error(DecodeStatus::Unsupported, e))?;
        let (samples, _) = buffer.as_mut().as_chunks_mut::<4>();
        transform.transform_in_place(samples);
    } else {
        for p in buffer.pixels_mut() {
            for c in 0..3 {
                p[c] = srgb_to_linear(p[c]);
            }
        }
    }
    Ok(DynamicImage::ImageRgba32F(buffer))
}
fn convert_cicp(image: &mut DynamicImage, primaries: u32, transfer: u32) -> Result<()> {
    if ![1, 2, 9, 12].contains(&primaries) || ![1, 2, 6, 8, 13, 16, 18].contains(&transfer) {
        return Err(error(
            DecodeStatus::Unsupported,
            format!("unsupported AVIF CICP {primaries}/{transfer}"),
        ));
    }
    let buffer = image.as_mut_rgba32f().unwrap();
    for p in buffer.pixels_mut() {
        for c in 0..3 {
            let v = p[c];
            p[c] = match transfer {
                8 => v,
                1 | 6 => {
                    if v < 0.081 {
                        v / 4.5
                    } else {
                        ((v + 0.099) / 1.099).powf(1.0 / 0.45)
                    }
                }
                16 => {
                    let x = v.powf(1.0 / 78.84375);
                    ((x - 0.8359375).max(0.0) / (18.851563 - 18.6875 * x)).powf(1.0 / 0.15930176)
                        * 10000.0
                        / 203.0
                }
                18 => {
                    if v <= 0.5 {
                        v * v / 3.0
                    } else {
                        ((v - 0.5599107) / 0.17883277)
                            .exp()
                            .mul_add(1.0, 0.28466892)
                            / 12.0
                    }
                }
                _ => srgb_to_linear(v),
            };
        }
        let rgb = [p[0], p[1], p[2]];
        let matrix = match primaries {
            9 => [
                [1.660491, -0.587641, -0.072850],
                [-0.124550, 1.132_9, -0.008349],
                [-0.018151, -0.100579, 1.118_73],
            ],
            12 => [
                [1.224_94, -0.224940, 0.0],
                [-0.042057, 1.042057, 0.0],
                [-0.019638, -0.078636, 1.098274],
            ],
            _ => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        };
        for c in 0..3 {
            p[c] = (0..3).map(|k| matrix[c][k] * rgb[k]).sum();
        }
    }
    Ok(())
}

// PNG gAMA/chromaticities are color metadata even in the absence of an ICC packet.
fn png_profile(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut offset = 8usize;
    let mut gamma = None;
    let mut chroma = None;
    let mut srgb = false;
    while let Some(header) = bytes.get(offset..offset + 8) {
        let n = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|x| x.checked_add(n))
            .ok_or_else(|| error(DecodeStatus::Corrupt, "PNG chunk overflow"))?;
        let data = bytes
            .get(offset + 8..end - 4)
            .ok_or_else(|| error(DecodeStatus::Corrupt, "truncated PNG chunk"))?;
        match &header[4..8] {
            b"sRGB" => srgb = true,
            b"gAMA" if n == 4 => {
                gamma = Some(u32::from_be_bytes(data.try_into().unwrap()) as f64 / 100000.0)
            }
            b"cHRM" if n == 32 => {
                let mut xy = [0.0; 8];
                for (i, b) in data.as_chunks::<4>().0.iter().enumerate() {
                    xy[i] = u32::from_be_bytes(*b) as f64 / 100000.0;
                }
                chroma = Some(xy);
            }
            b"cICP" => {
                return Err(error(
                    DecodeStatus::Unsupported,
                    "PNG CICP color conversion requires an explicit HDR policy",
                ));
            }
            b"IDAT" => break,
            _ => {}
        }
        offset = end;
    }
    if srgb || (gamma.is_none() && chroma.is_none()) {
        return Ok(None);
    }
    let gamma = gamma.unwrap_or(0.45455);
    if !(0.01..=10.0).contains(&gamma) {
        return Err(error(DecodeStatus::Corrupt, "invalid PNG gamma"));
    }
    let xy = chroma.unwrap_or([0.3127, 0.3290, 0.64, 0.33, 0.30, 0.60, 0.15, 0.06]);
    let point = |i| CIExyY {
        x: xy[i],
        y: xy[i + 1],
        Y: 1.0,
    };
    let curve = ToneCurve::new(1.0 / gamma);
    Profile::new_rgb(
        &point(0),
        &CIExyYTRIPLE {
            Red: point(2),
            Green: point(4),
            Blue: point(6),
        },
        &[&curve, &curve, &curve],
    )
    .and_then(|p| p.icc())
    .map(Some)
    .map_err(|e| error(DecodeStatus::Corrupt, e))
}
