//! N-only managed header/decode and verified RGB publication.
use super::*;
use crate::{
    application::U64,
    catalog_session::{
        LeaseId,
        native::{Header, Work},
    },
};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub operation: U64,
    pub stage: LeaseId,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rgb {
    pub bytes: U64,
    pub digest: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DecodedReceipt {
    pub header: Header,
    pub rgb: Rgb,
    pub peak_resident_bytes: Option<u64>,
    pub peak_method: String,
}
pub(super) fn write_rgb(index: usize, pixels: &PreparedRgb) -> Result<Rgb> {
    let bytes = pixels.pixels();
    write_exclusive(Path::new(&format!("{index}.rgb")), bytes)?;
    Ok(Rgb {
        bytes: U64(bytes.len() as u64),
        digest: blake3::hash(bytes).to_hex().to_string(),
    })
}
fn corrupt(message: &'static str) -> anyhow::Error {
    crate::media::DecodeError {
        status: crate::media::DecodeStatus::Corrupt,
        message: message.into(),
    }
    .into()
}
fn cache_header(
    bytes: &[u8],
    codec: Codec,
    expected_dimensions: Option<(u32, u32)>,
) -> Result<(u32, u32)> {
    let (width, height) = encoded_dimensions(bytes, codec)?;
    crate::catalog_session::native::rgb_bytes(width, height)
        .map_err(|_| corrupt("encoded cache dimensions exceed supported bounds"))?;
    if !expected_dimensions.is_none_or(|d| d == (width, height)) {
        return Err(corrupt(
            "cached record dimensions differ from encoded header",
        ));
    }
    Ok((width, height))
}
pub(super) fn decode_encoded(
    work: Work,
    binding: Binding,
    admitted: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let Work::DecodeEncoded {
        codec,
        encoded_bytes,
        encoded_digest,
        expected_dimensions,
    } = work
    else {
        bail!("wrong managed decode work")
    };
    // E is both the admission and actual owned capacity. The immutable file is
    // retained by F until this N and the subsequent C transfer have drained.
    let mut file = File::open("input.encoded")?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() == encoded_bytes.0,
        "managed input length changed"
    );
    let n = usize::try_from(encoded_bytes.0)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(n)?;
    bytes.resize(n, 0);
    file.read_exact(&mut bytes)?;
    let mut eof = [0];
    ensure!(file.read(&mut eof)? == 0, "managed input grew");
    ensure!(
        blake3::hash(&bytes).to_hex().as_str() == encoded_digest,
        "managed input digest changed"
    );
    let (width, height) = cache_header(&bytes, codec, expected_dimensions)?;
    let header = Header {
        operation: binding.operation,
        stage: binding.stage,
        input_digest: encoded_digest,
        input_bytes: encoded_bytes,
        codec,
        width,
        height,
    };
    header.validate()?;
    let metadata = serde_json::to_vec(&header)?;
    write_exclusive(Path::new("header.pending"), &metadata)?;
    fs::rename("header.pending", "header.ready")?;
    // HeaderReady is metadata only. No full decoder or RGB allocation precedes
    // the exact G grant backed by C's decoded-memory reservation.
    admitted.recv().context("managed RGB admission ended")?;
    let pixels = decode(&bytes, codec)?;
    if pixels.width() != width || pixels.height() != height {
        return Err(corrupt("cached decoded dimensions differ from header"));
    }
    let rgb = write_rgb(0, &pixels)?;
    drop(pixels);
    drop(bytes);
    drop(file);
    let (peak_resident_bytes, peak_method) = peak_resident_memory();
    let receipt = serde_json::to_vec(&DecodedReceipt {
        header,
        rgb,
        peak_resident_bytes,
        peak_method,
    })?;
    ensure!(
        receipt.len() as u64 <= RECEIPT_LIMIT,
        "managed decode receipt limit"
    );
    write_exclusive(Path::new("result.json"), &receipt)
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::preview::CodecSettings;
    #[test]
    fn cached_jpeg_trailing_bytes_preserve_legacy_decoder_acceptance() -> Result<()> {
        let image = PreparedRgb::new(2, 2, vec![17; 12])?;
        let mut bytes = encode(
            &image,
            CodecSettings {
                codec: Codec::Jpeg,
                quality: 80,
            },
            None,
        )?;
        bytes.extend_from_slice(b"legacy trailing metadata");
        let old = decode(&bytes, Codec::Jpeg)?;
        assert_eq!(
            cache_header(&bytes, Codec::Jpeg, None)?,
            (old.width(), old.height())
        );
        assert!(!bytes.ends_with(&[0xff, 0xd9]));
        Ok(())
    }
    #[test]
    fn cached_header_record_mismatch_is_corrupt_not_resource_or_transport() -> Result<()> {
        let image = PreparedRgb::new(2, 2, vec![17; 12])?;
        let bytes = encode(
            &image,
            CodecSettings {
                codec: Codec::Jpeg,
                quality: 80,
            },
            None,
        )?;
        let error = cache_header(&bytes, Codec::Jpeg, Some((3, 2))).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<crate::media::DecodeError>()
                .unwrap()
                .status,
            crate::media::DecodeStatus::Corrupt
        );
        Ok(())
    }
}
