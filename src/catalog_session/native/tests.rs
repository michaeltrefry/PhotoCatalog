use super::*;
use crate::{
    catalog_session::PhysicalObjectId,
    preview::{ByteBudget, CodecSettings, PreviewKey, ServiceLimits, Tier},
    storage_volume::NativePath,
};

fn root() -> RootCapability {
    // Synthetic identities only; these tests do not admit or open a catalog.
    #[cfg(unix)]
    let physical = PhysicalObjectId::Unix {
        device: U64(1),
        inode: U64(2),
    };
    #[cfg(windows)]
    let physical = PhysicalObjectId::Windows {
        volume_serial: U64(1),
        file_index: U64(2),
    };
    #[cfg(unix)]
    let path = std::path::Path::new("/synthetic");
    #[cfg(windows)]
    let path = std::path::Path::new(r"C:\synthetic");
    RootCapability {
        epoch: LeaseId::new(),
        token: LeaseId::new(),
        session: LeaseId::new(),
        canonical_root: NativePath::from_path(path),
        root_physical: physical,
        catalog_physical: physical,
    }
}

fn render() -> RenderWork {
    let key = |tier, edge, codec| PreviewKey {
        asset_id: "native-cost-fixture".into(),
        variant_id: "master".into(),
        generation: 1,
        image_pixel_generation: Some(1),
        fingerprint: "a".repeat(64),
        edit_revision: 0,
        renderer_version: crate::preview::renderer_identity().into(),
        preparation_version: crate::preview::PREPARATION_VERSION.into(),
        tier,
        edge,
        encoding: CodecSettings { codec, quality: 80 },
    };
    #[cfg(unix)]
    let path = std::path::Path::new("/synthetic/photo.raw");
    #[cfg(windows)]
    let path = std::path::Path::new(r"C:\synthetic\photo.raw");
    RenderWork {
        source: NativePath::from_path(path),
        keys: vec![
            key(Tier::Thumbnail, 256, Codec::Jpeg),
            key(Tier::Large, 512, Codec::Avif),
        ],
        encoded_limit: 1024 * 1024,
        decode_limits: crate::media::DecodeLimits::default(),
        edit: None,
    }
}

#[test]
fn small_cached_images_fit_dimension_based_admission_with_separate_rgb_ownership() -> Result<()> {
    // A worst-case 8192-square reservation rejects every one of these small
    // images. Admit the known planes, while C's RGB copy has its own owner.
    for (codec, width, height, budget) in [
        (Codec::Jpeg, 32, 24, 16 * 1024),
        (Codec::Webp, 64, 32, 32 * 1024),
        (Codec::Avif, 32, 24, 16 * 1024),
    ] {
        let native = ByteBudget::new(budget)?;
        let rgb = ByteBudget::new(rgb_bytes(width, height)?)?;
        let header = native.try_reserve(header_cost(1024, 1024)?).unwrap();
        assert!(native.used() < budget);
        drop(header);
        let required = decode_cost(codec, width, height, 1024, 1024, 4096)?;
        let native_owner = native
            .try_reserve(required)
            .expect("small preview should fit");
        let rgb_owner = rgb.try_reserve(rgb_bytes(width, height)?).unwrap();
        assert!(rgb.try_reserve(1).is_none());
        drop(native_owner);
        assert_eq!(native.used(), 0);
        assert_eq!(rgb.used(), u64::from(width) * u64::from(height) * 3);
        drop(rgb_owner);
        assert_eq!(rgb.used(), 0);
        assert!(
            ByteBudget::new(required - 1)?
                .try_reserve(required)
                .is_none()
        );
    }
    Ok(())
}

#[test]
fn header_and_render_phases_do_not_charge_retired_buffers_twice() -> Result<()> {
    // Header scratch can dominate a tiny decode; it does not coexist with the
    // full-decode scratch once the header parser has been destroyed.
    assert_eq!(decode_cost(Codec::Jpeg, 1, 1, 100, 20_000, 1)?, 20_100);
    // AVIF can have 16-bit YUV444+alpha before layout rejection, followed by
    // native RGB and Rust copy-out: 14 bytes per pixel, versus 11 for JPEG/WebP.
    assert_eq!(plane_bytes(Codec::Avif, 1, 1)?, 14);
    assert_eq!(plane_bytes(Codec::Jpeg, 1, 1)?, 11);
    assert_eq!(plane_bytes(Codec::Webp, 1, 1)?, 11);

    let request = render();
    request.validate()?;
    // Tier validation is sequential inside one native job. C transfer starts
    // after N drains, so adding both tiers' scratch or adding C transfer here
    // would unnecessarily reject this 4.75 MiB operation allowance.
    let cost = render_cost(&request, 1024 * 1024, 4096)?;
    let budget = ByteBudget::new(19 * 256 * 1024)?;
    let held = budget
        .try_reserve(cost)
        .expect("sequential phase maximum should fit");
    assert!(budget.used() >= 1024 * 1024 + 512 * 512 * 14);
    drop(held);
    assert_eq!(budget.used(), 0);
    Ok(())
}

#[test]
fn invalid_dimensions_zero_scratch_and_overflow_fail_before_reservation() -> Result<()> {
    for (width, height) in [(0, 1), (1, 0), (8193, 1), (1, u32::MAX)] {
        assert!(rgb_bytes(width, height).is_err());
        assert!(decode_cost(Codec::Avif, width, height, 1, 1, 1).is_err());
    }
    assert_eq!(rgb_bytes(8192, 8192)?, 201_326_592);
    assert!(header_cost(1, 0).is_err());
    assert!(header_cost(u64::MAX, 1).is_err());
    assert!(decode_cost(Codec::Jpeg, 1, 1, 1, 0, 1).is_err());
    assert!(decode_cost(Codec::Webp, 1, 1, 1, 1, 0).is_err());
    assert!(decode_cost(Codec::Avif, 1, 1, u64::MAX, 1, 1).is_err());
    assert!(decode_cost(Codec::Avif, 1, 1, 1, 1, u64::MAX).is_err());
    let request = render();
    assert!(render_cost(&request, 0, 1).is_err());
    assert!(render_cost(&request, 1, 0).is_err());
    assert!(render_cost(&request, u64::MAX, 1).is_err());
    assert!(render_cost(&request, 1, u64::MAX).is_err());
    Ok(())
}

#[test]
fn legacy_limits_keep_small_working_budget_when_scratch_defaults_are_added() -> Result<()> {
    let original = ServiceLimits {
        working_bytes: 4096,
        per_worker_bytes: 2048,
        ..ServiceLimits::default()
    };
    let mut legacy = serde_json::to_value(&original)?;
    let object = legacy.as_object_mut().unwrap();
    object.remove("cache_header_scratch_bytes");
    object.remove("cache_codec_scratch_bytes");
    let loaded: ServiceLimits = serde_json::from_value(legacy)?;
    assert_eq!(loaded.working_bytes, original.working_bytes);
    assert_eq!(loaded.per_worker_bytes, original.per_worker_bytes);
    assert_eq!(loaded.cache_header_scratch_bytes, DEFAULT_HEADER_SCRATCH);
    assert_eq!(loaded.cache_codec_scratch_bytes, DEFAULT_CODEC_SCRATCH);
    // Compatibility of the saved configuration is distinct from admission of
    // an individual job. Actual PreviewService::open is covered at that layer.
    let operation = header_cost(1, loaded.cache_header_scratch_bytes)?;
    assert!(
        ByteBudget::new(loaded.working_bytes)?
            .try_reserve(operation)
            .is_none()
    );
    Ok(())
}

#[test]
fn native_query_key_rejects_other_catalog_owners_and_preserves_large_operation() -> Result<()> {
    let root = root();
    let key = Key::new(&root, U64(9_007_199_254_740_993));
    key.validate()?;
    assert!(key.matches(&root));
    let query = Query {
        key,
        action: QueryAction::RetryDrain,
    };
    let bytes = serde_json::to_vec(&query)?;
    let decoded: Query = serde_json::from_slice(&bytes)?;
    assert_eq!(decoded, query);
    for field in [0, 1, 2] {
        let mut other = root.clone();
        match field {
            0 => other.epoch = LeaseId::new(),
            1 => other.session = LeaseId::new(),
            _ => other.token = LeaseId::new(),
        }
        assert!(!decoded.key.matches(&other));
    }
    assert!(Key::new(&root, U64(0)).validate().is_err());
    let mut unknown = serde_json::to_value(query)?;
    unknown["pid"] = 123.into();
    assert!(serde_json::from_value::<Query>(unknown).is_err());
    Ok(())
}

#[test]
fn encode_header_admits_dimensions_but_rejects_other_operation_or_malformed_identity() -> Result<()>
{
    let header = Header {
        operation: U64(7),
        stage: LeaseId::new(),
        input_digest: "a".repeat(64),
        input_bytes: U64(1024),
        codec: Codec::Avif,
        width: 32,
        height: 24,
    };
    let request = |h| Request {
        root: root(),
        operation: U64(7),
        action: Action::Encode {
            header: Some(h),
            working_bytes: U64(16 * 1024),
            rgb_bytes: U64(2304),
        },
    };
    request(header.clone()).validate()?;
    let mut stale = header.clone();
    stale.operation = U64(6);
    assert!(request(stale).validate().is_err());
    for field in [0, 1, 2, 3] {
        let mut malformed = header.clone();
        match field {
            0 => malformed.input_digest.push('a'),
            1 => malformed.input_bytes = U64(0),
            2 => malformed.width = 8193,
            _ => malformed.input_digest = "g".repeat(64),
        }
        assert!(request(malformed).validate().is_err());
    }
    // Stage, codec, digest, length and expected dimensions must also match the
    // immutable admitted Work in G; those checks require the owner fixture.
    Ok(())
}
