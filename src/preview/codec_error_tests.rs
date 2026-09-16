use super::*;
use crate::media::{DecodeError, DecodeStatus};

fn status(error: &anyhow::Error) -> Option<DecodeStatus> {
    error.downcast_ref::<DecodeError>().map(|e| e.status)
}

#[test]
fn cache_header_and_decode_reject_empty_mismatched_and_oversized_dimensions() -> Result<()> {
    let rgb = PreparedRgb::new(2, 2, vec![17; 12])?;
    let mut jpeg = encode(
        &rgb,
        CodecSettings {
            codec: Codec::Jpeg,
            quality: 80,
        },
        None,
    )?;
    for codec in [Codec::Jpeg, Codec::Webp, Codec::Avif] {
        assert_eq!(
            status(&encoded_dimensions(&[], codec).unwrap_err()),
            Some(DecodeStatus::Corrupt)
        );
        assert_eq!(
            status(&decode(&[], codec).unwrap_err()),
            Some(DecodeStatus::Corrupt)
        );
    }
    for codec in [Codec::Webp, Codec::Avif] {
        assert_eq!(
            status(&encoded_dimensions(&jpeg, codec).unwrap_err()),
            Some(DecodeStatus::Corrupt)
        );
        assert_eq!(
            status(&decode(&jpeg, codec).unwrap_err()),
            Some(DecodeStatus::Corrupt)
        );
    }
    let sof = jpeg
        .windows(2)
        .position(|b| b == [0xff, 0xc0])
        .expect("encoder emits SOF0");
    jpeg[sof + 7..sof + 9].copy_from_slice(&((MAX_EDGE + 1) as u16).to_be_bytes());
    assert_eq!(
        status(&encoded_dimensions(&jpeg, Codec::Jpeg).unwrap_err()),
        Some(DecodeStatus::Corrupt)
    );
    Ok(())
}

#[test]
fn actual_avif_header_and_decode_preserve_success_and_reject_truncation() -> Result<()> {
    let rgb = PreparedRgb::new(2, 2, vec![17; 12])?;
    let avif = encode(
        &rgb,
        CodecSettings {
            codec: Codec::Avif,
            quality: 80,
        },
        None,
    )?;
    assert_eq!(encoded_dimensions(&avif, Codec::Avif)?, (2, 2));
    let decoded = decode(&avif, Codec::Avif)?;
    assert_eq!((decoded.width(), decoded.height()), (2, 2));
    let truncated = &avif[..avif.len() / 2];
    assert_eq!(
        status(&encoded_dimensions(truncated, Codec::Avif).unwrap_err()),
        Some(DecodeStatus::Corrupt)
    );
    assert_eq!(
        status(&decode(truncated, Codec::Avif).unwrap_err()),
        Some(DecodeStatus::Corrupt)
    );
    Ok(())
}

#[test]
fn codec_resource_io_and_unknown_failures_cannot_authorize_invalidation() {
    let out = NativeBuffer::default();
    assert!(native_decode_status(&out, 0).is_ok());
    assert_eq!(
        status(&native_decode_status(&out, 2).unwrap_err()),
        Some(DecodeStatus::Corrupt)
    );
    assert_eq!(
        status(&native_decode_status(&out, 3).unwrap_err()),
        Some(DecodeStatus::ResourceLimit)
    );
    for code in [1, -1, 99] {
        assert_eq!(status(&native_decode_status(&out, code).unwrap_err()), None);
    }
    let memory = image::ImageError::Limits(image::error::LimitError::from_kind(
        image::error::LimitErrorKind::InsufficientMemory,
    ));
    assert_eq!(
        status(&image_decode_error(memory)),
        Some(DecodeStatus::ResourceLimit)
    );
    let io = image_decode_error(image::ImageError::IoError(std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "interrupted",
    )));
    assert_eq!(status(&io), None);
    assert!(matches!(
        io.downcast_ref::<image::ImageError>(),
        Some(image::ImageError::IoError(_))
    ));
    // An invalid successful native output is an internal failure, not evidence
    // that the encoded cache object is corrupt.
    assert_eq!(status(&native_result(&out, 0).unwrap_err()), None);
}

#[test]
fn wrapped_backend_errors_preserve_typed_resource_and_unknown_causes() {
    fn wrapped(error: impl std::error::Error + Send + Sync + 'static) -> anyhow::Error {
        image_decode_error(image::ImageError::Decoding(
            image::error::DecodingError::new(ImageFormat::WebP.into(), error),
        ))
    }
    use image_webp::DecodingError as Webp;
    for error in [Webp::MemoryLimitExceeded, Webp::ImageTooLarge] {
        assert_eq!(status(&wrapped(error)), Some(DecodeStatus::ResourceLimit));
    }
    for error in [
        Webp::InvalidParameter("caller".into()),
        Webp::UnsupportedFeature("feature".into()),
    ] {
        assert_eq!(status(&wrapped(error)), None);
    }
    assert_eq!(
        status(&wrapped(Webp::BitStreamError)),
        Some(DecodeStatus::Corrupt)
    );
    assert_eq!(
        status(&wrapped(Webp::IoError(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "io"
        )))),
        None
    );
    use zune_jpeg::errors::DecodeErrors as Jpeg;
    for error in [
        Jpeg::TooSmallOutput(12, 3),
        Jpeg::FormatStatic("opaque backend error"),
    ] {
        assert_eq!(status(&wrapped(error)), None);
    }
    assert_eq!(
        status(&wrapped(Jpeg::ExhaustedData)),
        Some(DecodeStatus::Corrupt)
    );
    assert_eq!(
        status(&wrapped(std::io::Error::other("opaque boxed failure"))),
        None
    );
}
