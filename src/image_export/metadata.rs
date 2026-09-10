use super::OutputDescriptor;
use crate::edit::RenderError;
use serde::{Deserialize, Serialize};
/// Only values whose offset-free representation is safe to reconstruct. Opaque
/// EXIF, MakerNotes, thumbnails and source IFD links are intentionally not inputs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafeExif {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub date_time_original: Option<String>,
    pub artist: Option<String>,
    pub copyright: Option<String>,
    pub description: Option<String>,
    pub exposure_time: Option<Rational>,
    pub f_number: Option<Rational>,
    pub iso: Option<u16>,
    pub focal_length: Option<Rational>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rational {
    pub numerator: u32,
    pub denominator: u32,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedExportMetadata {
    pub xmp: Option<String>,
    pub exif: SafeExif,
}
impl ResolvedExportMetadata {
    pub(crate) fn validate(&self, limit: u64) -> Result<(), RenderError> {
        let e = &self.exif;
        let mut n = self.xmp.as_ref().map_or(0, |x| x.len() as u64);
        for s in [
            &e.make,
            &e.model,
            &e.lens,
            &e.date_time_original,
            &e.artist,
            &e.copyright,
            &e.description,
        ]
        .into_iter()
        .flatten()
        {
            if s.len() > 8192 || s.bytes().any(|b| b == 0 || !b.is_ascii()) {
                return Err(RenderError::InvalidMetadata("safe EXIF ASCII value exceeds 8192 bytes or contains non-ASCII/NUL; retain Unicode in XMP".into()));
            }
            n += s.len() as u64;
        }
        for r in [e.exposure_time, e.f_number, e.focal_length]
            .into_iter()
            .flatten()
        {
            if r.denominator == 0 {
                return Err(RenderError::InvalidMetadata(
                    "zero EXIF rational denominator".into(),
                ));
            }
        }
        if n > limit {
            return Err(RenderError::ResourceLimit {
                resource: "export metadata",
                required: n,
                limit,
            });
        }
        if let Some(x) = &self.xmp {
            if x.contains('\0') {
                return Err(RenderError::InvalidMetadata("XMP contains NUL".into()));
            }
            x.parse::<xmp_toolkit::XmpMeta>()
                .map_err(|e| RenderError::InvalidMetadata(e.to_string()))?;
        }
        Ok(())
    }
}
#[derive(Clone)]
pub(crate) struct Entry {
    pub tag: u16,
    pub typ: u16,
    pub count: u32,
    pub bytes: Vec<u8>,
}
fn short(tag: u16, v: u16) -> Entry {
    Entry {
        tag,
        typ: 3,
        count: 1,
        bytes: v.to_le_bytes().to_vec(),
    }
}
fn long(tag: u16, v: u32) -> Entry {
    Entry {
        tag,
        typ: 4,
        count: 1,
        bytes: v.to_le_bytes().to_vec(),
    }
}
fn ascii(tag: u16, s: &str) -> Entry {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    Entry {
        tag,
        typ: 2,
        count: bytes.len() as u32,
        bytes,
    }
}
fn rational(tag: u16, r: Rational) -> Entry {
    let mut bytes = r.numerator.to_le_bytes().to_vec();
    bytes.extend(r.denominator.to_le_bytes());
    Entry {
        tag,
        typ: 5,
        count: 1,
        bytes,
    }
}
pub(crate) fn entries(e: &SafeExif, d: &OutputDescriptor) -> (Vec<Entry>, Vec<Entry>) {
    let mut root = vec![long(256, d.width), long(257, d.height), short(274, 1)];
    for (tag, value) in [
        (271, &e.make),
        (272, &e.model),
        (315, &e.artist),
        (33432, &e.copyright),
        (270, &e.description),
    ] {
        if let Some(v) = value {
            root.push(ascii(tag, v));
        }
    }
    let mut sub = vec![
        Entry {
            tag: 36864,
            typ: 7,
            count: 4,
            bytes: b"0232".to_vec(),
        },
        long(40962, d.width),
        long(40963, d.height),
    ];
    for (tag, value) in [(36867, &e.date_time_original), (42036, &e.lens)] {
        if let Some(v) = value {
            sub.push(ascii(tag, v));
        }
    }
    for (tag, value) in [
        (33434, e.exposure_time),
        (33437, e.f_number),
        (37386, e.focal_length),
    ] {
        if let Some(v) = value {
            sub.push(rational(tag, v));
        }
    }
    if let Some(v) = e.iso {
        sub.push(short(34855, v));
    }
    (root, sub)
}
fn append_ifd(out: &mut Vec<u8>, mut entries: Vec<Entry>) -> u32 {
    if out.len() % 2 != 0 {
        out.push(0);
    }
    let start = out.len();
    entries.sort_by_key(|e| e.tag);
    out.extend((entries.len() as u16).to_le_bytes());
    out.resize(start + 2 + entries.len() * 12 + 4, 0);
    for (i, e) in entries.into_iter().enumerate() {
        let p = start + 2 + i * 12;
        out[p..p + 2].copy_from_slice(&e.tag.to_le_bytes());
        out[p + 2..p + 4].copy_from_slice(&e.typ.to_le_bytes());
        out[p + 4..p + 8].copy_from_slice(&e.count.to_le_bytes());
        if e.bytes.len() <= 4 {
            out[p + 8..p + 8 + e.bytes.len()].copy_from_slice(&e.bytes);
        } else {
            if out.len() % 2 != 0 {
                out.push(0);
            }
            let offset = out.len() as u32;
            out[p + 8..p + 12].copy_from_slice(&offset.to_le_bytes());
            out.extend(&e.bytes);
        }
    }
    start as u32
}
/// A fresh little-endian TIFF-form EXIF packet, suitable for JPEG APP1 or PNG eXIf.
pub(crate) fn exif_packet(e: &SafeExif, d: &OutputDescriptor) -> Vec<u8> {
    let (mut root, sub) = entries(e, d);
    let mut out = b"II\x2a\0\0\0\0\0".to_vec();
    let sub_offset = append_ifd(&mut out, sub);
    root.push(long(34665, sub_offset));
    let root_offset = append_ifd(&mut out, root);
    out[4..8].copy_from_slice(&root_offset.to_le_bytes());
    out
}
