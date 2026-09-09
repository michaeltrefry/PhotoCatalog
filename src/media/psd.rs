//! Bounded PSD merged-composite reader. RGB/gray, 8/16/32-bit, raw/RLE/ZIP.
//! Layer records are skipped, never flattened using an invented blending model.
use anyhow::{Result, bail, ensure};
use image::{DynamicImage, Rgba32FImage};
use std::io::Read;
pub(super) struct Composite {
    pub image: DynamicImage,
    pub bits: u32,
    pub icc: Option<Vec<u8>>,
}
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(n)
            .ok_or_else(|| anyhow::anyhow!("PSD offset overflow"))?;
        let b = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| anyhow::anyhow!("truncated PSD"))?;
        self.offset = end;
        Ok(b)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn block(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}
/// Read source EXIF metadata without decoding the merged pixels.
pub(super) fn source_exif(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut r = Reader { bytes, offset: 26 };
    r.block()?;
    let resources = r.block()?;
    let mut rr = Reader {
        bytes: resources,
        offset: 0,
    };
    while rr.offset < resources.len() {
        ensure!(rr.take(4)? == b"8BIM", "invalid PSD resource signature");
        let id = rr.u16()?;
        let n = rr.take(1)?[0] as usize;
        rr.take(n)?;
        if !(n + 1).is_multiple_of(2) {
            rr.take(1)?;
        }
        let data = rr.block()?;
        if id == 1058 || id == 1059 {
            return Ok(Some(data.to_vec()));
        }
        if data.len() % 2 != 0 {
            rr.take(1)?;
        }
    }
    Ok(None)
}
pub(super) fn decode(bytes: &[u8], max_pixels: u64, max_allocation: u64) -> Result<Composite> {
    let mut r = Reader { bytes, offset: 0 };
    ensure!(r.take(4)? == b"8BPS", "invalid PSD signature");
    ensure!(r.u16()? == 1, "unsupported PSB version");
    r.take(6)?;
    let channels = r.u16()? as usize;
    let height = r.u32()?;
    let width = r.u32()?;
    let depth = r.u16()? as usize;
    let mode = r.u16()?;
    ensure!([8, 16, 32].contains(&depth), "unsupported PSD bit depth");
    ensure!([1, 3].contains(&mode), "unsupported PSD color mode {mode}");
    let colors = if mode == 1 { 1 } else { 3 };
    ensure!(
        channels >= colors && channels <= 56,
        "invalid PSD channel count"
    );
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow::anyhow!("PSD dimension overflow"))?;
    ensure!(
        width > 0 && height > 0 && width <= 40000 && height <= 40000 && pixels <= 100_000_000,
        "PSD resource limit"
    );
    ensure!(
        pixels as u64 <= max_pixels && (pixels as u64).saturating_mul(16) <= max_allocation,
        "PSD configured resource limit"
    );
    let bps = depth / 8;
    let plane = pixels
        .checked_mul(bps)
        .ok_or_else(|| anyhow::anyhow!("PSD plane overflow"))?;
    let total = plane
        .checked_mul(channels)
        .ok_or_else(|| anyhow::anyhow!("PSD channel overflow"))?;
    ensure!(
        total <= 1600 * 1024 * 1024 && total as u64 <= max_allocation,
        "PSD resource limit"
    );
    r.block()?;
    let resources = r.block()?;
    let mut rr = Reader {
        bytes: resources,
        offset: 0,
    };
    let mut icc = None;
    while rr.offset < resources.len() {
        ensure!(rr.take(4)? == b"8BIM", "unsupported PSD resource signature");
        let id = rr.u16()?;
        let n = rr.take(1)?[0] as usize;
        rr.take(n)?;
        if !(n + 1).is_multiple_of(2) {
            rr.take(1)?;
        }
        let data = rr.block()?;
        if id == 1057 {
            ensure!(data.len() >= 5, "truncated PSD VersionInfo");
            ensure!(
                data[4] != 0,
                "unsupported PSD without real merged composite"
            );
        }
        if id == 1039 {
            ensure!(data.len() <= 16 * 1024 * 1024, "ICC resource limit");
            icc = Some(data.to_vec());
        }
        if data.len() % 2 != 0 {
            rr.take(1)?;
        }
    }
    let layers = r.block()?;
    let mut merged_alpha = false;
    if !layers.is_empty() {
        let mut lr = Reader {
            bytes: layers,
            offset: 0,
        };
        let layer_info = lr.block()?;
        if !layer_info.is_empty() {
            ensure!(layer_info.len() >= 2, "truncated PSD layer count");
            merged_alpha = i16::from_be_bytes(layer_info[..2].try_into()?) < 0;
        }
        // Some writers omit the empty global mask when no additional blocks exist.
        if lr.offset < layers.len() {
            lr.block()?;
        }
        while lr.offset < layers.len() {
            // The whole layer/mask section may be padded to a four-byte boundary.
            let remaining = &layers[lr.offset..];
            if remaining.len() <= 3 && remaining.iter().all(|b| *b == 0) {
                break;
            }
            let signature = lr.take(4)?;
            ensure!(
                signature == b"8BIM" || signature == b"8B64",
                "invalid PSD layer tag signature"
            );
            let key = lr.take(4)?;
            let data = lr.block()?;
            if key == b"Mtrn" || key == b"Mt16" || key == b"Mt32" {
                ensure!(data.is_empty(), "invalid PSD merged transparency tag");
                merged_alpha = true;
            }
            if key == b"Layr" || key == b"Lr16" || key == b"Lr32" {
                ensure!(data.len() >= 2, "truncated PSD additional layer count");
                merged_alpha |= i16::from_be_bytes(data[..2].try_into()?) < 0;
            }
            if data.len() % 2 != 0 {
                lr.take(1)?;
            }
        }
    }
    ensure!(
        !merged_alpha || channels > colors,
        "PSD merged transparency channel missing"
    );
    let compression = r.u16()?;
    let mut data = match compression {
        0 => r.take(total)?.to_vec(),
        1 => {
            let rows = (height as usize)
                .checked_mul(channels)
                .ok_or_else(|| anyhow::anyhow!("PSD row overflow"))?;
            // Keep the row-size table in the admitted encoded buffer. Expanding
            // u16 lengths into a usize Vec can exceed a narrow image's allowance.
            let table_bytes = rows
                .checked_mul(2)
                .ok_or_else(|| anyhow::anyhow!("PSD row table overflow"))?;
            let sizes = r.take(table_bytes)?;
            let mut output = Vec::with_capacity(total);
            for size in sizes.chunks_exact(2) {
                let size = u16::from_be_bytes(size.try_into().unwrap()) as usize;
                let row = r.take(size)?;
                let mut i = 0;
                let start = output.len();
                let row_bytes = width as usize * bps;
                while i < row.len() {
                    let count = row[i] as i8;
                    i += 1;
                    match count {
                        0..=127 => {
                            let n = count as usize + 1;
                            ensure!(
                                output.len() - start + n <= row_bytes,
                                "PSD RLE row overflow"
                            );
                            output.extend_from_slice(
                                row.get(i..i + n)
                                    .ok_or_else(|| anyhow::anyhow!("truncated PSD RLE literal"))?,
                            );
                            i += n;
                        }
                        -127..=-1 => {
                            let n = (1 - count as i16) as usize;
                            ensure!(
                                output.len() - start + n <= row_bytes,
                                "PSD RLE row overflow"
                            );
                            let value = *row
                                .get(i)
                                .ok_or_else(|| anyhow::anyhow!("truncated PSD RLE repeat"))?;
                            i += 1;
                            output.resize(output.len() + n, value);
                        }
                        _ => {}
                    }
                }
                ensure!(output.len() - start == row_bytes, "truncated PSD RLE row");
            }
            output
        }
        2 | 3 => {
            // Exact allocation avoids geometric Vec growth crossing the admitted
            // plane limit. The stack byte still detects excess decompressed data.
            let mut output = vec![0; total];
            let mut decoder = flate2::read::ZlibDecoder::new(&bytes[r.offset..]);
            decoder.read_exact(&mut output)?;
            let mut extra = [0];
            ensure!(decoder.read(&mut extra)? == 0, "PSD ZIP size mismatch");
            output
        }
        _ => bail!("unsupported PSD compression {compression}"),
    };
    if compression == 3 {
        for row in data.chunks_exact_mut(width as usize * bps) {
            if depth == 16 {
                for x in 1..width as usize {
                    let prior = u16::from_be_bytes(row[(x - 1) * 2..x * 2].try_into()?);
                    let value =
                        u16::from_be_bytes(row[x * 2..x * 2 + 2].try_into()?).wrapping_add(prior);
                    row[x * 2..x * 2 + 2].copy_from_slice(&value.to_be_bytes());
                }
            } else {
                for x in 1..row.len() {
                    row[x] = row[x].wrapping_add(row[x - 1]);
                }
                if depth == 32 {
                    let source = row.to_vec();
                    for x in 0..width as usize {
                        for b in 0..4 {
                            row[x * 4 + b] = source[b * width as usize + x];
                        }
                    }
                }
            }
        }
    }
    let sample = |channel: usize, index: usize| -> f32 {
        let start = channel * plane + index * bps;
        match depth {
            8 => data[start] as f32 / 255.0,
            16 => u16::from_be_bytes(data[start..start + 2].try_into().unwrap()) as f32 / 65535.0,
            _ => f32::from_be_bytes(data[start..start + 4].try_into().unwrap()),
        }
    };
    let mut rgba = Rgba32FImage::new(width, height);
    for (i, p) in rgba.pixels_mut().enumerate() {
        for c in 0..3 {
            p[c] = sample(if colors == 1 { 0 } else { c }, i);
        }
        p[3] = if merged_alpha && channels > colors {
            sample(colors, i)
        } else {
            1.0
        };
        ensure!(
            p[3].is_finite() && (0.0..=1.0).contains(&p[3]),
            "invalid PSD alpha"
        );
        // Photoshop RGB merged composites are matted against white. Undo that
        // storage convention before ICC conversion to expose straight RGB.
        // At zero alpha the source foreground color cannot be recovered.
        if merged_alpha && mode == 3 && p[3] > 0.0 {
            for c in 0..3 {
                p[c] = (p[c] + p[3] - 1.0) / p[3];
            }
        }
    }
    Ok(Composite {
        image: DynamicImage::ImageRgba32F(rgba),
        bits: depth as u32,
        icc,
    })
}
