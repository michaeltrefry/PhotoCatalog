use super::*;
fn sample() -> ArtifactDescriptor {
    let identity = FileIdentity {
        object: "retained-copy".into(),
        bytes: u64::MAX,
        modified_ns: Some(u128::MAX),
        changed: "stamp".into(),
    };
    ArtifactDescriptor {
        protocol: 1,
        request: ArtifactRequest {
            retained_capture_record: i64::MAX,
            member_index: usize::MAX,
            mapping: ArtifactMapping {
                root: NativePath::WindowsWide(vec![55296, 0, 65535]),
                relative: NativePath::UnixBytes(vec![0, 255]),
                copy_identity: identity.clone(),
            },
        },
        selected_input: "a".repeat(64),
        capture_revision: "b".repeat(64),
        manifest_blake3: "c".repeat(64),
        artifact: Artifact {
            source: NativePath::UnixBytes(vec![255, 0]),
            role: "opaque".into(),
            relative: NativePath::WindowsWide(vec![55296]),
            stored: "raw/member".into(),
            revision: FileIdentity {
                object: "historical-original".into(),
                ..identity
            },
            blake3: "d".repeat(64),
        },
    }
}
fn parity(raw: &str, positive: bool) -> Result<()> {
    let old = serde_json::from_str::<ArtifactDescriptor>(raw);
    let new = decode(raw.as_bytes(), 32 * 1024 * 1024, &|| false);
    assert_eq!(old.is_ok(), positive, "public: {raw}");
    assert_eq!(new.is_ok(), positive, "private: {raw}");
    if let (Ok(a), Ok(b)) = (old, new) {
        assert_eq!(serde_json::to_vec(&a)?, serde_json::to_vec(&b)?);
        assert_eq!(a.request.mapping.copy_identity.object, "retained-copy");
        assert_eq!(b.artifact.revision.object, "historical-original");
    }
    Ok(())
}
#[test]
fn descriptor_full_fields_foreign_paths_and_representations_preserve_bytes() -> Result<()> {
    let d = sample();
    let raw = serde_json::to_string(&d)?;
    parity(&raw, true)?;
    parity(
        &serde_json::to_string(&(
            d.protocol,
            &d.request,
            &d.selected_input,
            &d.capture_revision,
            &d.manifest_blake3,
            &d.artifact,
        ))?,
        true,
    )?;
    parity(
        &raw.replace(
            "\"encoding\":\"WindowsWide\",\"units\":[55296,0,65535]",
            "\"units\":[55296,0,65535],\"encoding\":\"WindowsWide\"",
        ),
        true,
    )?;
    parity(&raw.replacen("{", "{\"protocol\":1,", 1), false)?;
    parity(&raw.replacen("{", "{\"unknown\":0,", 1), false)?;
    parity(
        &raw.replacen("\"artifact\":{", "\"artifact\":{\"unknown\":1e9999,", 1),
        true,
    )?;
    parity(
        &raw.replace(
            &u128::MAX.to_string(),
            "340282366920938463463374607431768211456",
        ),
        false,
    )?;
    Ok(())
}
#[test]
fn descriptor_paths_are_counted_before_allocation_and_canonical_limit_stays() -> Result<()> {
    let mut d = sample();
    d.artifact.source = NativePath::WindowsWide(vec![0; 30_000]);
    let bytes = serde_json::to_vec(&d)?;
    assert!(bytes.len() < DESCRIPTOR_LIMIT);
    let out = decode(&bytes, 32 * 1024 * 1024, &|| false)?;
    assert!(matches!(&out.artifact.source,NativePath::WindowsWide(v) if v.capacity()==30_000));
    assert_eq!(serde_json::to_vec(&out)?, bytes);
    println!(
        "C artifact descriptor size={} canonical={} requested path units={}",
        std::mem::size_of::<ArtifactDescriptor>(),
        bytes.len(),
        30_000
    );
    d.artifact.source = NativePath::WindowsWide(vec![0; 32_769]);
    let bytes = serde_json::to_vec(&d)?;
    assert!(serde_json::from_slice::<ArtifactDescriptor>(&bytes).is_ok());
    assert!(decode(&bytes, 32 * 1024 * 1024, &|| false).is_err());
    assert!(decode(&bytes, 32 * 1024 * 1024, &|| true).is_err());
    Ok(())
}
