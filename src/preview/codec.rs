use anyhow::{Context, Result, bail, ensure};
use image::{ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, c_char, c_void},
    io::Cursor,
    sync::atomic::{AtomicBool, Ordering},
};

pub const PREPARATION_VERSION: &str = "photocatalog-preview-srgb8-white-quantize-resize-1";
pub const MAX_EDGE: u32 = 8192;
pub const MAX_ENCODED: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    Jpeg,
    Webp,
    Avif,
}
impl Codec {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Avif => "avif",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CodecSettings {
    pub codec: Codec,
    pub quality: u8,
}
impl CodecSettings {
    pub fn validate(self) -> Result<()> {
        ensure!((1..=100).contains(&self.quality), "quality outside 1..100");
        Ok(())
    }
    pub fn effective(self) -> serde_json::Value {
        serde_json::json!({"codec":self.codec,"quality":self.quality,"preparation":PREPARATION_VERSION,"color":"opaque RGB8 sRGB; orientation baked", "configuration":match self.codec {
            Codec::Jpeg=>"image 0.25.9 JpegEncoder; YUV444 H/V1; sampling validated from SOF; image JPEG decode to_rgb8",
            Codec::Webp=>"libwebp DEFAULT preset; lossy; method4; thread_level0; YUV420; image-webp decode to_rgb8",
            Codec::Avif=>"libavif AOM encode/decode; speed6; threads1; 8bit YUV420 full; BT709 primaries/matrix; sRGB transfer; libyuv disabled; average downsample; bilinear upsample; no tiles/alpha/transform"
        }})
    }
}
/// Fully materialized, opaque sRGB. Owned bytes are included in request/cache accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRgb {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}
impl PreparedRgb {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self> {
        ensure!(
            width > 0 && height > 0 && width <= MAX_EDGE && height <= MAX_EDGE,
            "preview dimension limit"
        );
        ensure!(
            pixels.len() as u64 == u64::from(width) * u64::from(height) * 3,
            "RGB8 length mismatch"
        );
        Ok(Self {
            width,
            height,
            pixels,
        })
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
    pub fn byte_len(&self) -> usize {
        self.pixels.len()
    }
    pub fn digest(&self) -> String {
        let mut hash = blake3::Hasher::new();
        hash.update(&self.width.to_le_bytes());
        hash.update(&self.height.to_le_bytes());
        hash.update(&self.pixels);
        hash.finalize().to_hex().to_string()
    }
    pub fn save_reference_png(&self, path: &std::path::Path) -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        use image::ImageEncoder;
        let mut encoder = image::codecs::png::PngEncoder::new(&file);
        encoder.set_icc_profile(lcms2::Profile::new_srgb().icc()?)?;
        encoder.write_image(
            &self.pixels,
            self.width,
            self.height,
            image::ExtendedColorType::Rgb8,
        )?;
        file.sync_all()?;
        Ok(())
    }
}
pub fn prepare(rendered: &crate::media::RenderedImage, edge: u32) -> Result<PreparedRgb> {
    ensure!((1..=MAX_EDGE).contains(&edge), "preview edge limit");
    let rgb = rendered.srgb_preview_pixels(edge.min(rendered.width.max(rendered.height)))?;
    PreparedRgb::new(rgb.width(), rgb.height(), rgb.into_raw())
}
#[repr(C)]
struct NativeBuffer {
    data: *mut u8,
    len: usize,
    width: u32,
    height: u32,
    error: [c_char; 256],
}
impl Default for NativeBuffer {
    fn default() -> Self {
        Self {
            data: std::ptr::null_mut(),
            len: 0,
            width: 0,
            height: 0,
            error: [0; 256],
        }
    }
}
unsafe extern "C" {
    fn pc_preview_webp(
        rgb: *const u8,
        w: u32,
        h: u32,
        quality: i32,
        canceled: extern "C" fn(*const c_void) -> i32,
        context: *const c_void,
        out: *mut NativeBuffer,
    ) -> i32;
    fn pc_preview_avif(rgb: *const u8, w: u32, h: u32, quality: i32, out: *mut NativeBuffer)
    -> i32;
    fn pc_preview_avif_dimensions(data: *const u8, len: usize, out: *mut NativeBuffer) -> i32;
    fn pc_preview_avif_decode(data: *const u8, len: usize, out: *mut NativeBuffer) -> i32;
    fn pc_preview_free(out: *mut NativeBuffer);
    fn pc_preview_versions(out: *mut c_char, len: usize);
}
impl Drop for NativeBuffer {
    fn drop(&mut self) {
        unsafe { pc_preview_free(self) }
    }
}
extern "C" fn canceled(context: *const c_void) -> i32 {
    // The callback is synchronous; encode retains the borrowed AtomicBool for the call.
    i32::from(
        !context.is_null() && unsafe { &*context.cast::<AtomicBool>() }.load(Ordering::Relaxed),
    )
}
fn check_cancel(cancel: Option<&AtomicBool>) -> Result<()> {
    ensure!(
        !cancel.is_some_and(|v| v.load(Ordering::Relaxed)),
        "canceled"
    );
    Ok(())
}
fn native_result(out: &NativeBuffer, status: i32) -> Result<Vec<u8>> {
    if status != 0 {
        bail!(
            "{}",
            unsafe { CStr::from_ptr(out.error.as_ptr()) }.to_string_lossy()
        );
    }
    ensure!(
        !out.data.is_null() && out.len > 0 && out.len <= MAX_ENCODED,
        "native preview output limit"
    );
    Ok(unsafe { std::slice::from_raw_parts(out.data, out.len) }.to_vec())
}
/// CPU encoding only; no file I/O. WebP polls cancellation inside native work.
/// AVIF/JPEG callers needing hard cancellation must use an isolated worker.
pub fn encode(
    rgb: &PreparedRgb,
    settings: CodecSettings,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<u8>> {
    settings.validate()?;
    check_cancel(cancel)?;
    let bytes = match settings.codec {
        Codec::Jpeg => {
            let mut bytes = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, settings.quality)
                .encode(
                    &rgb.pixels,
                    rgb.width,
                    rgb.height,
                    image::ExtendedColorType::Rgb8,
                )?;
            bytes
        }
        Codec::Webp | Codec::Avif => {
            let mut out = NativeBuffer::default();
            let status = unsafe {
                match settings.codec {
                    Codec::Webp => pc_preview_webp(
                        rgb.pixels.as_ptr(),
                        rgb.width,
                        rgb.height,
                        i32::from(settings.quality),
                        canceled,
                        cancel.map_or(std::ptr::null(), |v| (v as *const AtomicBool).cast()),
                        &mut out,
                    ),
                    Codec::Avif => pc_preview_avif(
                        rgb.pixels.as_ptr(),
                        rgb.width,
                        rgb.height,
                        i32::from(settings.quality),
                        &mut out,
                    ),
                    _ => unreachable!(),
                }
            };
            native_result(&out, status)?
        }
    };
    check_cancel(cancel)?;
    ensure!(bytes.len() <= MAX_ENCODED, "encoded preview limit");
    Ok(bytes)
}
/// Parse dimensions without allocating decoded pixel planes. Callers use this
/// before reserving output memory; codec scratch is a separate worker allowance.
pub fn encoded_dimensions(bytes: &[u8], codec: Codec) -> Result<(u32, u32)> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_ENCODED,
        "encoded preview limit"
    );
    let (width, height) = if codec == Codec::Avif {
        let mut out = NativeBuffer::default();
        let status = unsafe { pc_preview_avif_dimensions(bytes.as_ptr(), bytes.len(), &mut out) };
        ensure!(
            status == 0,
            "AVIF header: {}",
            unsafe { CStr::from_ptr(out.error.as_ptr()) }.to_string_lossy()
        );
        (out.width, out.height)
    } else {
        let format = match codec {
            Codec::Jpeg => ImageFormat::Jpeg,
            Codec::Webp => ImageFormat::WebP,
            _ => unreachable!(),
        };
        ensure!(
            image::guess_format(bytes)? == format,
            "preview codec mismatch"
        );
        ImageReader::with_format(Cursor::new(bytes), format).into_dimensions()?
    };
    ensure!(
        width > 0 && height > 0 && width <= MAX_EDGE && height <= MAX_EDGE,
        "preview dimension limit"
    );
    Ok((width, height))
}
/// Complete production cache decoder: allocation and conversion to owned RGB8 are included.
/// Input is an internally generated cache object, not an arbitrary original/photo decoder.
pub fn decode(bytes: &[u8], codec: Codec) -> Result<PreparedRgb> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_ENCODED,
        "encoded preview limit"
    );
    if codec == Codec::Avif {
        let mut out = NativeBuffer::default();
        let status = unsafe { pc_preview_avif_decode(bytes.as_ptr(), bytes.len(), &mut out) };
        let pixels = native_result(&out, status)?;
        return PreparedRgb::new(out.width, out.height, pixels);
    }
    let format = match codec {
        Codec::Jpeg => ImageFormat::Jpeg,
        Codec::Webp => ImageFormat::WebP,
        _ => unreachable!(),
    };
    ensure!(
        image::guess_format(bytes).context("preview header")? == format,
        "preview codec mismatch"
    );
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(MAX_ENCODED as u64);
    reader.limits(limits);
    let rgb = reader.decode()?.to_rgb8();
    PreparedRgb::new(rgb.width(), rgb.height(), rgb.into_raw())
}
pub fn versions() -> String {
    let mut native = [0 as c_char; 1024];
    unsafe { pc_preview_versions(native.as_mut_ptr(), native.len()) };
    let lock = include_str!("../../Cargo.lock");
    let version = |name: &str| {
        lock.split("[[package]]")
            .find(|entry| entry.lines().any(|line| line == format!("name = {name:?}")))
            .and_then(|entry| {
                entry
                    .lines()
                    .find_map(|line| line.strip_prefix("version = "))
            })
            .unwrap_or("unknown")
    };
    format!(
        "image={};image-webp={};{}",
        version("image"),
        version("image-webp"),
        unsafe { CStr::from_ptr(native.as_ptr()) }.to_string_lossy()
    )
}
