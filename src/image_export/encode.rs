use super::{
    metadata::{Entry, entries, exif_packet},
    specification::output_profile,
    *,
};
use crate::edit::{CancelCheck, EditedLinearImage, RenderError};
use image::ImageEncoder;
use lcms2::{Flags, Intent, PixelFormat, Profile, Transform};
use std::{
    borrow::Cow,
    io::{self, Seek, SeekFrom, Write},
};
use tiff::encoder::{
    DirectoryEncoder, TiffEncoder, TiffKindStandard, TiffValue, colortype,
    compression::DeflateLevel,
};
use tiff::tags::{Tag, Type};
#[derive(Debug, Clone, serde::Serialize)]
pub struct EncodingReport {
    pub output: OutputDescriptor,
    pub encoded_extent: u64,
    pub source_fingerprint: String,
    pub recipe_digest: String,
    pub metadata_blake3: String,
    pub compression: String,
}
fn codec(e: impl std::fmt::Display) -> RenderError {
    RenderError::Codec(e.to_string())
}
struct CancelWriter<'a, W> {
    sink: &'a mut W,
    cancel: &'a dyn CancelCheck,
}
impl<W: Write> Write for CancelWriter<'_, W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.cancel.is_canceled() {
            return Err(io::Error::new(io::ErrorKind::Other, "export canceled"));
        }
        self.sink.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}
impl<W: Seek> Seek for CancelWriter<'_, W> {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        if self.cancel.is_canceled() {
            return Err(io::Error::new(io::ErrorKind::Other, "export canceled"));
        }
        self.sink.seek(p)
    }
}
struct Rows<'a> {
    pixels: &'a [[f32; 4]],
    descriptor: &'a OutputDescriptor,
    transform: Option<Transform<[f32; 4], [f32; 4]>>,
    srgb: bool,
    row: Vec<[f32; 4]>,
}
impl<'a> Rows<'a> {
    fn new(
        pixels: &'a [[f32; 4]],
        descriptor: &'a OutputDescriptor,
        spec: &OutputSpec,
        target: &Profile,
        limits: EncodeLimits,
    ) -> Result<Self, RenderError> {
        let bytes = u64::from(descriptor.width) * 16;
        if bytes > limits.row_buffer_bytes {
            return Err(RenderError::ResourceLimit {
                resource: "color row",
                required: bytes,
                limit: limits.row_buffer_bytes,
            });
        }
        let transform = if matches!(spec.profile, OutputProfile::Icc { .. }) {
            let source = crate::media::linear_profile().map_err(codec)?;
            Some(
                Transform::new_flags(
                    &source,
                    PixelFormat::RGBA_FLT,
                    target,
                    PixelFormat::RGBA_FLT,
                    Intent::RelativeColorimetric,
                    Flags::COPY_ALPHA | Flags::NO_OPTIMIZE,
                )
                .map_err(codec)?,
            )
        } else {
            None
        };
        Ok(Self {
            pixels,
            descriptor,
            transform,
            srgb: matches!(spec.profile, OutputProfile::Srgb),
            row: crate::edit::buffer(descriptor.width as usize, limits.render)?,
        })
    }
    fn at(&mut self, y: u32) -> Result<&[[f32; 4]], RenderError> {
        let width = self.descriptor.width as usize;
        self.row
            .copy_from_slice(&self.pixels[y as usize * width..(y as usize + 1) * width]);
        if let AlphaPolicy::Composite { linear_rgb } = self.descriptor.alpha {
            for p in &mut self.row {
                for c in 0..3 {
                    p[c] = p[c] * p[3] + linear_rgb[c] * (1.0 - p[3]);
                }
                p[3] = 1.0;
            }
        }
        if let Some(t) = &self.transform {
            t.transform_in_place(&mut self.row);
        } else if self.srgb {
            for p in &mut self.row {
                for c in &mut p[..3] {
                    *c = if *c <= 0.0031308 {
                        12.92 * *c
                    } else {
                        1.055 * c.powf(1.0 / 2.4) - 0.055
                    };
                }
            }
        }
        if self.row.iter().flatten().any(|v| !v.is_finite()) {
            return Err(RenderError::InvalidProfile(
                "profile conversion produced nonfinite components".into(),
            ));
        }
        Ok(&self.row)
    }
}
fn q8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}
fn q16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
}
/// Encode into an empty caller-owned bounded staging sink. Cancellation is
/// checked per scanline and at each write/seek; native codec calls themselves
/// remain cooperative and must run in the parent's cancellable worker process.
pub fn encode_export<W: Write + Seek>(
    image: &EditedLinearImage,
    spec: &OutputSpec,
    metadata: &ResolvedExportMetadata,
    sink: &mut BoundedSeekWriter<W>,
    limits: EncodeLimits,
    cancel: &dyn CancelCheck,
) -> Result<EncodingReport, RenderError> {
    cancel.check()?;
    if sink.extent() != 0 || sink.stream_position()? != 0 {
        return Err(RenderError::InvalidOutput(
            "export staging sink must be empty at offset zero".into(),
        ));
    }
    let d = describe_output(image, spec)?;
    metadata.validate(limits.max_metadata_bytes)?;
    let source = image.as_rendered();
    limits.render.admit(source.width, source.height, 1)?;
    limits.render.admit(d.width, d.height, 4)?;
    limits.render.admit(source.width, source.height, 4)?;
    let resized;
    if (d.width, d.height) == (source.width, source.height) {
        resized = None;
    } else {
        resized = Some(crate::edit::geometry::resize(
            &source.pixels,
            source.width,
            source.height,
            d.width,
            d.height,
            limits.render,
            cancel,
        )?);
    }
    let pixels = resized.as_deref().unwrap_or(&source.pixels);
    let (target, icc, _) = output_profile(&spec.profile)?;
    if blake3::hash(&icc).to_hex().as_str() != d.icc_blake3 {
        return Err(RenderError::InvalidProfile(
            "profile identity changed during output description".into(),
        ));
    }
    let mut rows = Rows::new(pixels, &d, spec, &target, limits)?;
    let mut writer = CancelWriter { sink, cancel };
    let result = match spec.format {
        OutputFormat::Jpeg { quality } => jpeg(
            &mut writer,
            &mut rows,
            quality,
            metadata,
            &icc,
            limits,
            cancel,
        ),
        OutputFormat::Png { .. } => png_image(&mut writer, &mut rows, metadata, &icc, cancel),
        OutputFormat::Tiff { .. } => {
            tiff_image(&mut writer, &mut rows, metadata, &icc, limits, cancel)
        }
    };
    cancel.check()?;
    if writer.sink.exceeded() {
        return Err(RenderError::ResourceLimit {
            resource: "encoded extent",
            required: writer.sink.limit().saturating_add(1),
            limit: writer.sink.limit(),
        });
    }
    result?;
    writer.flush()?;
    let compression = match spec.format {
        OutputFormat::Jpeg { quality } => format!("JPEG image-rs quality={quality}; 4:2:2"),
        OutputFormat::Png { .. } => "PNG fast/adaptive".into(),
        OutputFormat::Tiff { depth } => format!(
            "TIFF deflate6; predictor={}",
            if depth == TiffDepth::Float32 {
                "none"
            } else {
                "horizontal"
            }
        ),
    };
    let metadata_bytes = serde_json::to_vec(metadata).map_err(codec)?;
    Ok(EncodingReport {
        output: d,
        encoded_extent: sink.extent(),
        source_fingerprint: image.source_fingerprint().into(),
        recipe_digest: image.recipe_digest().into(),
        metadata_blake3: blake3::hash(&metadata_bytes).to_hex().to_string(),
        compression,
    })
}
fn jpeg_headers(metadata: &ResolvedExportMetadata) -> Result<Vec<u8>, RenderError> {
    let mut headers = Vec::new();
    if let Some(x) = &metadata.xmp {
        let x = x.parse::<xmp_toolkit::XmpMeta>().map_err(codec)?;
        let (standard, extended, digest) = x.package_for_jpeg().map_err(codec)?;
        let mut app = |payload: &[u8]| -> Result<(), RenderError> {
            if payload.len() > 65533 {
                return Err(RenderError::InvalidMetadata(
                    "JPEG APP1 payload exceeds marker limit".into(),
                ));
            }
            headers.extend([0xff, 0xe1]);
            headers.extend(((payload.len() + 2) as u16).to_be_bytes());
            headers.extend(payload);
            Ok(())
        };
        let mut payload = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
        payload.extend(standard.as_bytes());
        app(&payload)?;
        if !extended.is_empty() {
            if digest.len() != 32 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(RenderError::InvalidMetadata(
                    "SDK returned invalid extended XMP digest".into(),
                ));
            }
            let prefix = b"http://ns.adobe.com/xmp/extension/\0";
            let chunk = 65533 - prefix.len() - 32 - 8;
            let total = u32::try_from(extended.len()).map_err(codec)?;
            for (i, part) in extended.as_bytes().chunks(chunk).enumerate() {
                let mut payload = prefix.to_vec();
                payload.extend(digest.as_bytes());
                payload.extend(total.to_be_bytes());
                payload.extend(((i * chunk) as u32).to_be_bytes());
                payload.extend(part);
                app(&payload)?;
            }
        }
    }
    Ok(headers)
}
/// Insert XMP only after verifying the encoder's SOI, without buffering its file.
struct JpegMux<W> {
    inner: W,
    headers: Vec<u8>,
    start: Vec<u8>,
    published: bool,
}
impl<W: Write> Write for JpegMux<W> {
    fn write(&mut self, mut b: &[u8]) -> io::Result<usize> {
        let count = b.len();
        if !self.published {
            let n = (2 - self.start.len()).min(b.len());
            self.start.extend(&b[..n]);
            b = &b[n..];
            if self.start.len() < 2 {
                return Ok(count);
            }
            if self.start != [0xff, 0xd8] {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "JPEG encoder omitted SOI",
                ));
            }
            self.inner.write_all(&self.start)?;
            self.inner.write_all(&self.headers)?;
            self.published = true;
        }
        self.inner.write_all(b)?;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
fn jpeg<W: Write>(
    writer: &mut W,
    rows: &mut Rows<'_>,
    quality: u8,
    m: &ResolvedExportMetadata,
    icc: &[u8],
    limits: EncodeLimits,
    cancel: &dyn CancelCheck,
) -> Result<(), RenderError> {
    let d = rows.descriptor;
    let count = u64::from(d.width) * u64::from(d.height) * 3;
    if count > limits.render.max_allocation_bytes {
        return Err(RenderError::ResourceLimit {
            resource: "JPEG RGB input",
            required: count,
            limit: limits.render.max_allocation_bytes,
        });
    }
    let mut rgb = Vec::new();
    rgb.try_reserve_exact(count as usize)
        .map_err(|_| RenderError::ResourceLimit {
            resource: "JPEG RGB allocation",
            required: count,
            limit: limits.render.max_allocation_bytes,
        })?;
    for y in 0..d.height {
        cancel.check()?;
        for p in rows.at(y)? {
            rgb.extend([q8(p[0]), q8(p[1]), q8(p[2])]);
        }
    }
    let mux = JpegMux {
        inner: writer,
        headers: jpeg_headers(m)?,
        start: Vec::with_capacity(2),
        published: false,
    };
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(mux, quality);
    encoder.set_icc_profile(icc.to_vec()).map_err(codec)?;
    let exif = exif_packet(&m.exif, d);
    if exif.len() + 6 > 65533 {
        return Err(RenderError::InvalidMetadata(
            "safe EXIF exceeds JPEG APP1; shorten selected fields".into(),
        ));
    }
    encoder.set_exif_metadata(exif).map_err(codec)?;
    encoder
        .encode(&rgb, d.width, d.height, image::ExtendedColorType::Rgb8)
        .map_err(codec)
}
fn png_image<W: Write>(
    writer: &mut W,
    rows: &mut Rows<'_>,
    m: &ResolvedExportMetadata,
    icc: &[u8],
    cancel: &dyn CancelCheck,
) -> Result<(), RenderError> {
    let d = rows.descriptor;
    let mut info = png::Info::with_size(d.width, d.height);
    info.color_type = if d.channels == 4 {
        png::ColorType::Rgba
    } else {
        png::ColorType::Rgb
    };
    info.bit_depth = if d.bits_per_sample == 8 {
        png::BitDepth::Eight
    } else {
        png::BitDepth::Sixteen
    };
    info.icc_profile = Some(Cow::Borrowed(icc));
    info.exif_metadata = Some(Cow::Owned(exif_packet(&m.exif, d)));
    let mut encoder = png::Encoder::with_info(writer, info).map_err(codec)?;
    encoder.set_compression(png::Compression::Fast);
    encoder.set_filter(png::Filter::Adaptive);
    if let Some(x) = &m.xmp {
        encoder
            .add_itxt_chunk("XML:com.adobe.xmp".into(), x.clone())
            .map_err(codec)?;
    }
    let mut writer = encoder.write_header().map_err(codec)?;
    let mut stream = writer.stream_writer().map_err(codec)?;
    let mut bytes = Vec::with_capacity(d.width as usize * d.channels as usize * 2);
    for y in 0..d.height {
        cancel.check()?;
        bytes.clear();
        for p in rows.at(y)? {
            for v in &p[..d.channels as usize] {
                if d.bits_per_sample == 8 {
                    bytes.push(q8(*v));
                } else {
                    bytes.extend(q16(*v).to_be_bytes());
                }
            }
        }
        stream.write_all(&bytes)?;
    }
    stream.finish().map_err(codec)?;
    writer.finish().map_err(codec)
}
struct Undefined<'a>(&'a [u8]);
impl TiffValue for Undefined<'_> {
    const BYTE_LEN: u8 = 1;
    const FIELD_TYPE: Type = Type::UNDEFINED;
    fn count(&self) -> usize {
        self.0.len()
    }
    fn data(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(self.0)
    }
}
fn tags<W: Write + Seek>(
    dir: &mut DirectoryEncoder<'_, W, TiffKindStandard>,
    values: Vec<Entry>,
) -> Result<(), RenderError> {
    for e in values {
        let tag = Tag::Unknown(e.tag);
        match e.typ {
            2 => dir
                .write_tag(
                    tag,
                    std::str::from_utf8(&e.bytes[..e.bytes.len() - 1]).map_err(codec)?,
                )
                .map_err(codec)?,
            3 => dir
                .write_tag(tag, u16::from_le_bytes(e.bytes[..2].try_into().unwrap()))
                .map_err(codec)?,
            4 => dir
                .write_tag(tag, u32::from_le_bytes(e.bytes[..4].try_into().unwrap()))
                .map_err(codec)?,
            5 => dir
                .write_tag(
                    tag,
                    tiff::encoder::Rational {
                        n: u32::from_le_bytes(e.bytes[..4].try_into().unwrap()),
                        d: u32::from_le_bytes(e.bytes[4..8].try_into().unwrap()),
                    },
                )
                .map_err(codec)?,
            7 => dir.write_tag(tag, Undefined(&e.bytes)).map_err(codec)?,
            _ => {
                return Err(RenderError::InvalidMetadata(
                    "unsupported generated EXIF type".into(),
                ));
            }
        }
    }
    Ok(())
}
fn tiff_image<W: Write + Seek>(
    writer: &mut W,
    rows: &mut Rows<'_>,
    m: &ResolvedExportMetadata,
    icc: &[u8],
    limits: EncodeLimits,
    cancel: &dyn CancelCheck,
) -> Result<(), RenderError> {
    let d = rows.descriptor;
    let raw_bytes = u64::from(d.width) * u64::from(d.channels) * u64::from(d.bits_per_sample / 8);
    let compression_bound = raw_bytes
        .checked_mul(2)
        .and_then(|n| n.checked_add(1024))
        .ok_or_else(|| RenderError::InvalidOutput("TIFF row overflow".into()))?;
    let table_bytes = u64::from(d.height) * 4;
    for (resource, required, limit) in [
        (
            "TIFF compressed scanline",
            compression_bound,
            limits
                .row_buffer_bytes
                .min(limits.render.max_allocation_bytes),
        ),
        (
            "TIFF strip table",
            table_bytes,
            limits.render.max_allocation_bytes,
        ),
    ] {
        if required > limit {
            return Err(RenderError::ResourceLimit {
                resource,
                required,
                limit,
            });
        }
    }
    let mut encoder = TiffEncoder::new(writer).map_err(codec)?;
    let (root, sub) = entries(&m.exif, d);
    let mut exif = encoder.extra_directory().map_err(codec)?;
    tags(&mut exif, sub)?;
    let exif_offset = exif.finish_with_offsets().map_err(codec)?.offset;
    let mut dir = encoder.image_directory().map_err(codec)?;
    tags(&mut dir, root)?;
    dir.write_tag(Tag::ExifDirectory, exif_offset)
        .map_err(codec)?;
    dir.write_tag(Tag::Unknown(34675), icc).map_err(codec)?;
    if let Some(x) = &m.xmp {
        dir.write_tag(Tag::Unknown(700), x.as_bytes())
            .map_err(codec)?;
    }
    if d.channels == 4 {
        dir.write_tag(Tag::ExtraSamples, 2u16).map_err(codec)?;
    }
    dir.write_tag(
        Tag::BitsPerSample,
        vec![u16::from(d.bits_per_sample); d.channels as usize].as_slice(),
    )
    .map_err(codec)?;
    dir.write_tag(
        Tag::SampleFormat,
        vec![if d.floating_point { 3u16 } else { 1u16 }; d.channels as usize].as_slice(),
    )
    .map_err(codec)?;
    dir.write_tag(Tag::PhotometricInterpretation, 2u16)
        .map_err(codec)?;
    dir.write_tag(Tag::SamplesPerPixel, u16::from(d.channels))
        .map_err(codec)?;
    dir.write_tag(Tag::RowsPerStrip, 1u32).map_err(codec)?;
    dir.write_tag(Tag::PlanarConfiguration, 1u16)
        .map_err(codec)?;
    dir.write_tag(Tag::Compression, 8u16).map_err(codec)?;
    dir.write_tag(Tag::Predictor, if d.floating_point { 1u16 } else { 2u16 })
        .map_err(codec)?;
    let mut offsets = Vec::with_capacity(d.height as usize);
    let mut counts = Vec::with_capacity(d.height as usize);
    let mut raw = Vec::with_capacity(raw_bytes as usize);
    for y in 0..d.height {
        cancel.check()?;
        raw.clear();
        let mut previous = [0u16; 4];
        for p in rows.at(y)? {
            for c in 0..d.channels as usize {
                match d.bits_per_sample {
                    8 => {
                        let v = q8(p[c]);
                        raw.push(v.wrapping_sub(previous[c] as u8));
                        previous[c] = u16::from(v);
                    }
                    16 => {
                        let v = q16(p[c]);
                        raw.extend(v.wrapping_sub(previous[c]).to_ne_bytes());
                        previous[c] = v;
                    }
                    32 => raw.extend(p[c].to_ne_bytes()),
                    _ => return Err(RenderError::InvalidOutput("unsupported TIFF depth".into())),
                }
            }
        }
        // tiff 0.10 write_strip does not enable its compressor. Compress this bounded
        // scanline explicitly, then let the mature directory writer own all offsets.
        let mut staging = BoundedSeekWriter::new(
            std::io::Cursor::new(Vec::with_capacity(compression_bound as usize)),
            compression_bound,
        )?;
        {
            let mut zip =
                flate2::write::ZlibEncoder::new(&mut staging, flate2::Compression::new(6));
            zip.write_all(&raw)?;
            zip.finish()?;
        }
        let encoded = staging.into_inner().into_inner();
        let offset = dir.write_data(encoded.as_slice()).map_err(codec)?;
        offsets.push(u32::try_from(offset).map_err(codec)?);
        counts.push(u32::try_from(encoded.len()).map_err(codec)?);
    }
    dir.write_tag(Tag::StripOffsets, offsets.as_slice())
        .map_err(codec)?;
    dir.write_tag(Tag::StripByteCounts, counts.as_slice())
        .map_err(codec)?;
    dir.finish().map_err(codec)
}
