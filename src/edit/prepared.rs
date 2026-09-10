//! Private-cache transport for developed linear proxies. This is never an
//! original/exact export input, even when a small source fits entirely in a proxy.
use super::*;
use std::io::{Read, Write};
const MAGIC: &[u8; 8] = b"PCF32P1\0";
const MAX_HEADER: u64 = 128 * 1024;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedProxyIdentity {
    pub source_fingerprint: String,
    pub white_balance: WhiteBalance,
    pub renderer_identity: String,
    pub original_dimensions: (u32, u32),
    pub longest_edge: u32,
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedProxyReceipt {
    pub bytes: u64,
    pub blake3: String,
    pub identity: PreparedProxyIdentity,
}
pub struct PreparedProxyExpectation<'a> {
    pub source_fingerprint: &'a str,
    pub white_balance: &'a WhiteBalance,
    pub longest_edge: u32,
    pub original_dimensions: (u32, u32),
    pub blake3: &'a str,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    identity: PreparedProxyIdentity,
    metadata: crate::media::Metadata,
    provenance: crate::media::RenderProvenance,
}
#[derive(Serialize)]
struct HeaderRef<'a> {
    identity: &'a PreparedProxyIdentity,
    metadata: &'a crate::media::Metadata,
    provenance: &'a crate::media::RenderProvenance,
}
struct HeaderWriter {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for HeaderWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if b.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("prepared header limit"));
        }
        self.bytes.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn valid_identity(i: &PreparedProxyIdentity) -> Result<(), RenderError> {
    if i.renderer_identity != renderer_identity() {
        return Err(RenderError::InvalidInput(
            "prepared proxy renderer differs".into(),
        ));
    }
    if i.source_fingerprint.len() != 64
        || !i.source_fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(RenderError::InvalidInput(
            "prepared proxy fingerprint is not BLAKE3".into(),
        ));
    }
    if i.longest_edge == 0 || i.longest_edge > 40000 {
        return Err(RenderError::InvalidInput(
            "prepared proxy edge outside supported bounds".into(),
        ));
    }
    let recipe = super::super::Recipe::V1(super::super::RecipeV1 {
        white_balance: i.white_balance.clone(),
        ..Default::default()
    })
    .validate()?;
    recipe.validate_dimensions(i.original_dimensions.0, i.original_dimensions.1)?;
    if geometry::fit(
        i.original_dimensions.0,
        i.original_dimensions.1,
        i.longest_edge,
        i.longest_edge,
        false,
    )? != (i.width, i.height)
    {
        return Err(RenderError::InvalidInput(
            "prepared proxy dimensions differ from declared edge/source".into(),
        ));
    }
    Ok(())
}
fn length(header: u64, i: &PreparedProxyIdentity, max_bytes: u64) -> Result<u64, RenderError> {
    let bytes = u64::from(i.width) * u64::from(i.height) * 16 + 12 + header + 32;
    if bytes > max_bytes {
        return Err(RenderError::ResourceLimit {
            resource: "prepared proxy bytes",
            required: bytes,
            limit: max_bytes,
        });
    }
    Ok(bytes)
}
pub fn write_prepared_proxy<W: Write>(
    input: &PreparedLinearInput,
    longest_edge: u32,
    writer: &mut W,
    max_bytes: u64,
    cancel: &dyn CancelCheck,
) -> Result<PreparedProxyReceipt, RenderError> {
    cancel.check()?;
    if input.exact {
        return Err(RenderError::InvalidInput(
            "prepare an explicit linear proxy before persistence".into(),
        ));
    }
    let identity = PreparedProxyIdentity {
        source_fingerprint: input.source_fingerprint.clone(),
        white_balance: input.white_balance.clone(),
        renderer_identity: input.renderer_identity.clone(),
        original_dimensions: input.original_dimensions,
        longest_edge,
        width: input.width(),
        height: input.height(),
    };
    valid_identity(&identity)?;
    let cap = MAX_HEADER.min(max_bytes.saturating_sub(44)) as usize;
    let mut sink = HeaderWriter {
        bytes: Vec::with_capacity(cap),
        limit: cap,
    };
    serde_json::to_writer(
        &mut sink,
        &HeaderRef {
            identity: &identity,
            metadata: &input.image.metadata,
            provenance: &input.image.provenance,
        },
    )
    .map_err(|_| RenderError::ResourceLimit {
        resource: "prepared proxy header",
        required: cap as u64 + 1,
        limit: cap as u64,
    })?;
    let header = sink.bytes;
    let total = length(header.len() as u64, &identity, max_bytes)?;
    let mut hash = blake3::Hasher::new();
    for b in [
        MAGIC.as_slice(),
        &(header.len() as u32).to_le_bytes(),
        header.as_slice(),
    ] {
        writer.write_all(b)?;
        hash.update(b);
    }
    let mut buffer = [0u8; 65536];
    for chunk in input.pixels().chunks(4096) {
        cancel.check()?;
        for (p, out) in chunk.iter().zip(buffer.as_chunks_mut::<16>().0.iter_mut()) {
            for (v, bytes) in p.iter().zip(out.as_chunks_mut::<4>().0.iter_mut()) {
                bytes.copy_from_slice(&v.to_le_bytes());
            }
        }
        let b = &buffer[..chunk.len() * 16];
        writer.write_all(b)?;
        hash.update(b);
    }
    let checksum = *hash.finalize().as_bytes();
    writer.write_all(&checksum)?;
    hash.update(&checksum);
    cancel.check()?;
    Ok(PreparedProxyReceipt {
        bytes: total,
        blake3: hash.finalize().to_hex().to_string(),
        identity,
    })
}
/// The caller first establishes that the source revision still matches its
/// catalog authority. Loading performs no source I/O and verifies both the
/// immutable entry hash and every declared linear pixel before exposing it.
pub fn read_prepared_proxy<R: Read>(
    reader: &mut R,
    expected: PreparedProxyExpectation<'_>,
    max_bytes: u64,
    limits: RenderLimits,
    cancel: &dyn CancelCheck,
) -> Result<PreparedLinearInput, RenderError> {
    cancel.check()?;
    if max_bytes < 44 {
        return Err(RenderError::ResourceLimit {
            resource: "prepared proxy bytes",
            required: 44,
            limit: max_bytes,
        });
    }
    let mut prefix = [0u8; 12];
    reader.read_exact(&mut prefix)?;
    if &prefix[..8] != MAGIC {
        return Err(RenderError::InvalidInput(
            "unknown prepared proxy container/version".into(),
        ));
    }
    let header_len = u32::from_le_bytes(prefix[8..].try_into().unwrap()) as u64;
    let cap = MAX_HEADER
        .min(max_bytes.saturating_sub(44))
        .min(limits.max_allocation_bytes);
    if header_len > cap {
        return Err(RenderError::ResourceLimit {
            resource: "prepared proxy header",
            required: header_len,
            limit: cap,
        });
    }
    let mut header = vec![0; header_len as usize];
    reader.read_exact(&mut header)?;
    let parsed: Header =
        serde_json::from_slice(&header).map_err(|e| RenderError::InvalidInput(e.to_string()))?;
    let i = &parsed.identity;
    valid_identity(i)?;
    if i.source_fingerprint != expected.source_fingerprint
        || &i.white_balance != expected.white_balance
        || i.longest_edge != expected.longest_edge
        || i.original_dimensions != expected.original_dimensions
    {
        return Err(RenderError::InvalidInput(
            "prepared proxy source/WB/edge identity differs".into(),
        ));
    }
    length(header_len, i, max_bytes)?;
    limits.admit(i.width, i.height, 1)?;
    let mut pixels = super::super::buffer(i.width as usize * i.height as usize, limits)?;
    let mut hash = blake3::Hasher::new();
    hash.update(&prefix);
    hash.update(&header);
    let mut buffer = [0u8; 65536];
    for chunk in pixels.chunks_mut(4096) {
        cancel.check()?;
        let b = &mut buffer[..chunk.len() * 16];
        reader.read_exact(b)?;
        hash.update(b);
        for (p, input) in chunk.iter_mut().zip(b.as_chunks::<16>().0.iter()) {
            for (v, bytes) in p.iter_mut().zip(input.as_chunks::<4>().0.iter()) {
                *v = f32::from_le_bytes(*bytes);
            }
            if p.iter().any(|v| !v.is_finite()) || !(0.0..=1.0).contains(&p[3]) {
                return Err(RenderError::InvalidInput(
                    "invalid prepared linear pixel/alpha".into(),
                ));
            }
        }
    }
    let mut checksum = [0u8; 32];
    reader.read_exact(&mut checksum)?;
    if hash.finalize().as_bytes() != &checksum {
        return Err(RenderError::InvalidInput(
            "prepared proxy content checksum differs".into(),
        ));
    }
    hash.update(&checksum);
    if hash.finalize().to_hex().as_str() != expected.blake3 {
        return Err(RenderError::InvalidInput(
            "prepared proxy entry hash differs".into(),
        ));
    }
    let mut extra = [0];
    if reader.read(&mut extra)? != 0 {
        return Err(RenderError::InvalidInput(
            "prepared proxy trailing bytes".into(),
        ));
    }
    cancel.check()?;
    Ok(PreparedLinearInput {
        image: RenderedImage {
            width: i.width,
            height: i.height,
            pixels,
            metadata: parsed.metadata,
            provenance: parsed.provenance,
        },
        source_fingerprint: i.source_fingerprint.clone(),
        white_balance: i.white_balance.clone(),
        renderer_identity: i.renderer_identity.clone(),
        original_dimensions: i.original_dimensions,
        exact: false,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn source() -> PreparedLinearInput {
        let mut p = fixture(2, 1, vec![[3., -0.5, 0.25, 0.5], [0.1, 0.2, 0.3, 0.]]);
        p.source_fingerprint = blake3::hash(b"original").to_hex().to_string();
        prepare_linear_proxy(&p, 2, RenderLimits::default(), &()).unwrap()
    }
    fn expect<'a>(i: &'a PreparedProxyIdentity, h: &'a str) -> PreparedProxyExpectation<'a> {
        PreparedProxyExpectation {
            source_fingerprint: &i.source_fingerprint,
            white_balance: &i.white_balance,
            longest_edge: i.longest_edge,
            original_dimensions: i.original_dimensions,
            blake3: h,
        }
    }
    #[test]
    fn lossless_proxy_roundtrip_is_bound_and_never_exact() {
        let p = source();
        let mut bytes = Vec::new();
        let receipt = write_prepared_proxy(&p, 2, &mut bytes, 1_000_000, &()).unwrap();
        assert_eq!(receipt.bytes, bytes.len() as u64);
        assert_eq!(receipt.blake3, blake3::hash(&bytes).to_hex().to_string());
        let loaded = read_prepared_proxy(
            &mut Cursor::new(&bytes),
            expect(&receipt.identity, &receipt.blake3),
            1_000_000,
            RenderLimits::default(),
            &(),
        )
        .unwrap();
        assert_eq!(loaded.pixels(), p.pixels());
        assert!(!loaded.exact());
        assert!(
            render_recipe(
                &loaded,
                &super::super::super::Recipe::default().validate().unwrap(),
                RenderPurpose::ExportExact,
                RenderLimits::default(),
                &()
            )
            .is_err()
        );
    }
    #[test]
    fn rejects_corruption_wrong_identity_limits_and_truncation() {
        let p = source();
        let mut bytes = Vec::new();
        let r = write_prepared_proxy(&p, 2, &mut bytes, 1_000_000, &()).unwrap();
        let load = |b: &[u8], i: &PreparedProxyIdentity, limit| {
            read_prepared_proxy(
                &mut Cursor::new(b),
                expect(i, &r.blake3),
                limit,
                RenderLimits::default(),
                &(),
            )
        };
        assert!(load(&bytes, &r.identity, 10).is_err());
        let mut other = r.identity.clone();
        other.longest_edge = 3;
        assert!(load(&bytes, &other, 1_000_000).is_err());
        let mut corrupt = bytes.clone();
        let index = corrupt.len() - 40;
        corrupt[index] ^= 1;
        assert!(load(&corrupt, &r.identity, 1_000_000).is_err());
        assert!(load(&bytes[..bytes.len() - 1], &r.identity, 1_000_000).is_err());
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(load(&extra, &r.identity, 1_000_000).is_err());
    }
}
