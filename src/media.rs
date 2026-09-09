mod full;
mod psd;
pub use full::{
    ColorCalibration, DecodeError, DecodeStatus, RenderProvenance, RenderedImage, decode_full,
    decoder_versions,
};

use anyhow::{Context, Result, bail, ensure};
use image::{DynamicImage, ImageReader};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufReader, Cursor, Read, Seek, SeekFrom},
    path::Path,
};

const MAX_ENCODED: u64 = 128 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub orientation: u32,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub captured_at: Option<String>,
    #[serde(default)]
    pub lens: Option<String>,
    pub preview_source: String,
}

pub fn supported_extension(ext: &str) -> bool {
    [
        "cr2", "cr3", "dng", "raf", "rw2", "nef", "arw", "orf", "pef", "jpg", "jpeg", "png",
        "avif", "webp", "bmp", "tif", "tiff", "psd",
    ]
    .contains(&ext)
}

pub fn decode(path: &Path) -> Result<(Metadata, Vec<u8>)> {
    // An explicitly labelled embedded preview is allowed for browsing. Editing always
    // enters decode_full, which never reads this preview or a catalog thumbnail.
    let mut header = [0u8; 12];
    let mut file = File::open(path)?;
    let n = file.read(&mut header)?;
    if n == 12 && &header[8..12] == b"CR\x02\0" {
        return decode_embedded_cr2(path);
    }
    let rendered = decode_full(path)?;
    let preview = rendered.srgb_preview(512)?;
    Ok((rendered.metadata, preview))
}

fn decode_embedded_cr2(path: &Path) -> Result<(Metadata, Vec<u8>)> {
    let mut file = File::open(path)?;
    let mut header = [0; 16];
    let n = file.read(&mut header)?;
    file.rewind()?;
    let is_cr2 = n >= 16
        && (&header[..4] == b"II*\0" || &header[..4] == b"MM\0*")
        && &header[8..12] == b"CR\x02\0";
    let (encoded, format, source, raw_tags) = if is_cr2 {
        let (jpeg, tags) = cr2_preview(&mut file)?;
        (
            jpeg,
            "CR2",
            "embedded JPEG (not RAW development)",
            Some(tags),
        )
    } else {
        ensure!(
            file.metadata()?.len() <= MAX_ENCODED,
            "encoded image exceeds 128 MiB skeleton limit"
        );
        let mut data = Vec::new();
        file.take(MAX_ENCODED + 1).read_to_end(&mut data)?;
        ensure!(data.len() as u64 <= MAX_ENCODED, "source grew beyond limit");
        let format = match image::guess_format(&data)? {
            image::ImageFormat::Jpeg => "JPEG",
            image::ImageFormat::Png => "PNG",
            _ => bail!("format not supported by skeleton decoder"),
        };
        (data, format, "decoded original", None)
    };
    let exif = exif::Reader::new()
        .read_from_container(&mut BufReader::new(Cursor::new(&encoded)))
        .ok();
    let text = |tag| {
        exif.as_ref()
            .and_then(|e| e.get_field(tag, exif::In::PRIMARY))
            .map(|f| f.display_value().to_string())
    };
    let raw = raw_tags.unwrap_or_default();
    let orientation = raw
        .orientation
        .or_else(|| {
            exif.as_ref()
                .and_then(|e| e.get_field(exif::Tag::Orientation, exif::In::PRIMARY))
                .and_then(|f| f.value.get_uint(0))
        })
        .unwrap_or(1);
    ensure!((1..=8).contains(&orientation), "invalid EXIF orientation");
    let mut reader = ImageReader::new(Cursor::new(&encoded)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(40_000);
    limits.max_image_height = Some(40_000);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().context("decode preview image")?;
    let metadata = Metadata {
        format: format.into(),
        width: raw.width.unwrap_or(decoded.width()),
        height: raw.height.unwrap_or(decoded.height()),
        orientation,
        camera_make: raw.make.or_else(|| text(exif::Tag::Make)),
        camera_model: raw.model.or_else(|| text(exif::Tag::Model)),
        captured_at: raw.date.or_else(|| text(exif::Tag::DateTimeOriginal)),
        lens: raw.lens.or_else(|| text(exif::Tag::LensModel)),
        preview_source: source.into(),
    };
    let thumbnail = orient(
        decoded.thumbnail(decoded.width().min(512), decoded.height().min(512)),
        orientation,
    );
    let mut preview = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut preview, 65)
        .encode_image(&thumbnail.to_rgb8())?;
    Ok((metadata, preview))
}
fn orient(image: DynamicImage, orientation: u32) -> DynamicImage {
    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        _ => image,
    }
}
#[derive(Default)]
struct RawTags {
    lens: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    orientation: Option<u32>,
    make: Option<String>,
    model: Option<String>,
    date: Option<String>,
}
// Follow bounded TIFF directory entries, never scan RAW bytes for JPEG markers.
fn cr2_preview(file: &mut File) -> Result<(Vec<u8>, RawTags)> {
    let len = file.metadata()?.len();
    let mut head = [0; 16];
    file.read_exact(&mut head)?;
    let little = &head[..2] == b"II";
    let u16_at = |b: &[u8]| {
        if little {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    };
    let u32_at = |b: &[u8]| {
        if little {
            u32::from_le_bytes(b[..4].try_into().unwrap())
        } else {
            u32::from_be_bytes(b[..4].try_into().unwrap())
        }
    };
    let primary_offset = u32_at(&head[4..8]);
    let mut queue = vec![primary_offset];
    let mut seen = std::collections::HashSet::new();
    let mut candidates = Vec::new();
    let mut tags = RawTags::default();
    while let Some(offset) = queue.pop() {
        if offset == 0 {
            continue;
        }
        ensure!(seen.insert(offset), "cyclic CR2 TIFF directory");
        ensure!(seen.len() <= 32, "too many CR2 TIFF directories");
        ensure!(
            u64::from(offset) + 2 <= len,
            "TIFF directory outside source"
        );
        file.seek(SeekFrom::Start(offset.into()))?;
        let mut count = [0; 2];
        file.read_exact(&mut count)?;
        let count = u16_at(&count) as usize;
        ensure!(count <= 4096, "too many TIFF entries");
        ensure!(
            u64::from(offset) + 2 + (count * 12 + 4) as u64 <= len,
            "truncated TIFF directory"
        );
        let mut entries = vec![0; count * 12 + 4];
        file.read_exact(&mut entries)?;
        let mut compression = 0;
        let mut strip_offset = None;
        let mut strip_len = None;
        let mut jpeg_offset = None;
        let mut jpeg_len = None;
        for entry in entries[..count * 12].as_chunks::<12>().0 {
            let tag = u16_at(entry);
            let kind = u16_at(&entry[2..]);
            let count = u32_at(&entry[4..]);
            let val = if count == 1 && kind == 3 {
                Some(u16_at(&entry[8..]) as u32)
            } else if count == 1 && kind == 4 {
                Some(u32_at(&entry[8..]))
            } else {
                None
            };
            match tag {
                256 if offset == primary_offset => {
                    tags.width = val;
                }
                40962 => {
                    if val.is_some() {
                        tags.width = val;
                    }
                }
                257 if offset == primary_offset => {
                    tags.height = val;
                }
                40963 => {
                    if val.is_some() {
                        tags.height = val;
                    }
                }
                274 if offset == primary_offset => {
                    tags.orientation = val;
                }
                259 => compression = val.unwrap_or(0),
                273 => strip_offset = val,
                279 => strip_len = val,
                513 => jpeg_offset = val,
                514 => jpeg_len = val,
                34665 => {
                    if let Some(v) = val {
                        queue.push(v);
                    }
                }
                271 | 272 | 36867 | 42036 if kind == 2 && count > 0 && count <= 4096 => {
                    let bytes = if count <= 4 {
                        entry[8..8 + count as usize].to_vec()
                    } else {
                        let position = u32_at(&entry[8..]) as u64;
                        ensure!(position + count as u64 <= len, "TIFF text outside source");
                        file.seek(SeekFrom::Start(position))?;
                        let mut bytes = vec![0; count as usize];
                        file.read_exact(&mut bytes)?;
                        bytes
                    };
                    let value = String::from_utf8_lossy(&bytes)
                        .trim_end_matches('\0')
                        .to_string();
                    match tag {
                        271 => tags.make = Some(value),
                        272 => tags.model = Some(value),
                        42036 => tags.lens = Some(value),
                        _ => tags.date = Some(value),
                    }
                }
                _ => {}
            }
        }
        if let (Some(o), Some(n)) = (jpeg_offset, jpeg_len) {
            candidates.push((o, n));
        }
        if offset == primary_offset
            && compression == 6
            && let (Some(o), Some(n)) = (strip_offset, strip_len)
        {
            candidates.push((o, n));
        }
        queue.push(u32_at(&entries[count * 12..]));
    }
    candidates.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (offset, length) in candidates {
        if length == 0 || length as u64 > MAX_ENCODED || offset as u64 + length as u64 > len {
            continue;
        }
        file.seek(SeekFrom::Start(offset.into()))?;
        let mut data = vec![0; length as usize];
        file.read_exact(&mut data)?;
        if data.starts_with(b"\xff\xd8\xff") {
            return Ok((data, tags));
        }
    }
    bail!("CR2 has no supported bounded embedded JPEG preview")
}
